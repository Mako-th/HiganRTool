//! 台本の、ゲームのスレッドの層と台本のファイルの層（unsafe はここにもある。1 つずつ `// SAFETY:`）。
//!
//! - 入力の入口（`stub.rs` の `input_wait_entry`）は、`SCRIPT_ON` が 0 でなければ、tick の記録を書いた後に `script_on_tick(k)` を呼ぶ。
//!   戻り値が 0 なら元の飛び先（poll_all）へ、1 なら poll_all を呼ばずに戻る。中は全部 `catch_unwind`。判断は `scripted::tick`（pure）に
//!   だけ書き、ここはゲームのメモリの読み・ゲームの関数の呼び出し・台本の入れ物・進み具合の原子変数・feed のリングバッファへの書き・撮影の要求だけを持つ。
//!   ゲームのスレッドの経路で、確保・ファイル・ロック待ち・`unwrap` をしない。
//! - 台本の置き場: DLL のフォルダの `script.txt`。読んだら本ログの隣の `<回の名前>_script.txt` へ上書きせずに移す（`MoveFileExW` を旗 0 で。
//!   `std::fs::rename` は上書きの旗で呼ぶので使わない）。ローダーの `--script` / `--ini` が環境の変数で渡したパスがあればそちらを読み、
//!   渡された台本は移さない（2 つのゲームがそれぞれのコピーを持てるように）。
//! - **ネットワーク**: ネットワークを置いた実行では、入口の分岐は台本とネットワークの両方の入口になる。`script_on_tick` はまずネットワークの判断（`netplay::NetPlay::on_input`）を
//!   呼び、`Local` なら台本の経路（終わっていれば 0）、`Zero` / `Played` なら 1、`RenderOnly` なら 2 を返す。ネットワークのセッション（`NET`）に触るのは
//!   ゲームのスレッドだけ。台本が無くてもネットワークは開ける（`arm_io`）。`after_script = keep` なら、台本が最後の段階まで押してもネットワークを閉じない。
//! - **観戦者の側**: 「ルームで観戦」で、開くスレッドが観戦者のネットワークを作り（`open_watch_net`）、ゲームのスレッドが郵便受け `PENDING` から取って
//!   **後から**置く（`install_placed`）。待っているだけの対戦者のセッションの上から決めたときは、先にセッションを手放して（`release_slot`）ワーカーのスレッドが
//!   片づけ、続けて開く（`SPECTATE_AFTER`）。観戦が終わり、マッチの外へ戻り、ルームのスレッドが書き終えたらセッションを手放す（`watch_teardown`）。
//! - **マッチの状態の操作**: `tick_inner` は、どの経路を通った tick でも最後に `state_tick` で台本の無敵・ライフ・ゲージを書く（判断は
//!   `state_ops::apply`）。Player はヒープなので落ちない手段（`OwnProcess` / `WriteProcessMemory`）で読み書きし、ネットワークか巻き戻しの実験が
//!   動いている実行では書かずに断る（`state_ops::Refuse`）。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "script_win.rs は 32 ビット（i686）専用（ゲームの関数と 32 ビットのアドレスを前提にしている）"
);

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use higanr_core::script::{Engine, Script, Status};
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThreadId};

use crate::feeds::{FEED_RING, IM_BYTES};
use crate::menu::MemRead;
use crate::menu_win::OwnProcess;
use crate::names::{is_same_or_inside, side_file_name};
use std::sync::{Arc, OnceLock};

use higanr_core::lang::Lang;
use higanr_core::mode_pick::Shape;
use higanr_core::net_menu::{CodeState, Conn, Event, Note, RoomItem, sanitize_room};
use higanr_core::player_name::NameBuf;
use higanr_core::room_input::{InitialFrom, RoomBuf};
use higanr_core::save_skip::{
    self, Clear, Marked, Refuse as SkipRefuse, Side as SkipSide, SkipMem, Watch,
};
use higanr_core::wire::Seat;

use crate::clipboard::{Clipboard, FileClipboard};
use crate::clipboard_win::SystemClipboard;
use crate::net_code;
use crate::net_code_game::{self, GameUi};
use crate::net_config::{AfterScript, ClipboardKind, CodeSide, NetSettings};
use crate::net_game::{self, NetGame, NetIo};
use crate::net_portrait::PortraitCall;
use crate::net_room::RoomOptions;
use crate::net_udp::{self, GameNet, RecvThread};
use crate::net_ui::{self, BOARD, HudFacts, NetPhase, NetUi, UiFacts, UiLink, UiOut};
use crate::net_watch::{NET_FRAMES, NET_STATUS, NetSnapshot};
use crate::netplay::{self, Action, NetPlay};
use crate::scripted::{self, GameAddrs, Progress, RunState, TickIo};
use crate::session::ScriptHost;
use crate::shot_win;
use crate::spectate_host;
use crate::spectate_join::{self, JoinKit, Publisher};
use crate::spectate_watch::{WATCH_MARGIN_FRAMES, WATCH_STATUS, WatchConfig, WatchPlay, WatchSlot};
use crate::spectate_watch_code::{self, Reader};
use crate::state_ops::{self, PLAYER_SLOTS, Refuse, STATE_LOG, StateMem};
use crate::stub;
use crate::th09_addr::{
    CONTINUED_BIT, D3D8_DEVICE_PTR, D3D8_PRESENT_PARAMS, MENU_SCREEN_ID, RNG_ADDR, SAVE_SKIP_SITE,
    SUP_SCENE_CUR,
};
use crate::trial_summary::{HostFacts, MenuNet, OpenInfo};
use crate::win::read_own_memory;

/// 台本が動いているか。入力の入口のアセンブリが `cmp dword ptr [SCRIPT_ON], 0` で見る。0 → 1 は読み取りのスレッドだけ（`start`）、
/// 1 → 0 はゲームのスレッド（台本が終わった・止まった・パニック）と検査の後始末。
pub static SCRIPT_ON: AtomicU32 = AtomicU32::new(0);

/// ゲームのスレッドが読むアドレス（`GameAddrs` の並び）と、呼ぶ関数のアドレス（feed・ローカル入力）。`arm` が
/// `SCRIPT_ON` を 1 にする前に置く。
static ADDRS: [AtomicU32; 10] = [const { AtomicU32::new(0) }; 10];
static FEED_FN: AtomicU32 = AtomicU32::new(0);
static LOCAL_FN: AtomicU32 = AtomicU32::new(0);
/// 台本の状態の操作がたどる Player へのポインタの欄（1P, 2P）。`arm_io` が花映塚のアドレスを置く（検査は偽の欄へ置き直す）。
static STATE_SLOTS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];

struct EngineCell(UnsafeCell<Option<Engine>>);
// SAFETY: 中身に触るのは 2 か所だけで、同時には触らない。読み取りのスレッドの `arm` は `SCRIPT_ON` が 0 の間だけ置き
// （本番では 2 つのフックを書く前に 1 回だけ。検査では `REAL_HOOK_TESTS` のロックの下で 1 本ずつ）、以後はゲームのスレッドだけが触る。`start` は Release、
// ゲームのスレッドは Acquire で読んでから触る。
unsafe impl Sync for EngineCell {}
static ENGINE: EngineCell = EngineCell(UnsafeCell::new(None));

static STATE: AtomicU32 = AtomicU32::new(0);
static STEP: AtomicU32 = AtomicU32::new(0);
static LINE: AtomicU32 = AtomicU32::new(0);
static TICKS: AtomicU32 = AtomicU32::new(0);
static FED: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
pub(crate) static PANIC_NEXT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// 検査だけ: 0 でなければ、次の `script_on_tick` はこの値 − 1 を返す（入口のアセンブリの戻り値の経路を見る）。
#[cfg(test)]
pub(crate) static FORCE_RET: AtomicU32 = AtomicU32::new(0);
/// 検査だけ: `match_start_on` が呼ばれた回数（開始のフックの入口が `NET_ON` を見て呼ぶかを数える）。
#[cfg(test)]
pub(crate) static MATCH_ON_CALLS: AtomicU32 = AtomicU32::new(0);

// ---- ネットワーク ----

/// ネットワークが動いているか。開始のフックの入口のアセンブリが `cmp dword ptr [NET_ON], 0` で見る。0 → 1 は読み取りのスレッドだけ（`start_net`）、
/// 1 → 0 はゲームのスレッド（パニック）。
pub static NET_ON: AtomicU32 = AtomicU32::new(0);
static SET_STATE_FN: AtomicU32 = AtomicU32::new(0);
/// 立ち絵の 3 本の関数の入口（`net_portrait::PortraitFn::index` の順）。`session::run` が 3 本のバイト列を全部照合して合ったときだけ
/// `arm_portrait` が置く（0 のままなら呼ばない）。
static PORTRAIT_FNS: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
/// 素の poll `0x0042B850` の入口。起動のときに `session::run` が先頭 8 バイトを照合して合ったときだけ `arm_poll` が置く
/// （0 のままなら、ローカル入力は `LOCAL_FN` = `0x0042BE40` で読む）。
static POLL_FN: AtomicU32 = AtomicU32::new(0);
static GAME_TID: AtomicU32 = AtomicU32::new(0);
pub static MATCH_FOREIGN: AtomicU32 = AtomicU32::new(0);
pub static NET_PANICKED: AtomicU32 = AtomicU32::new(0);
/// 1 なら、rollback のモードのフックがネットワークのセッション `NET` と台本の入れ物 `ENGINE` を `&mut` で持っている間。フックが呼ぶ走査の中で入れ子に走る入力の入口と
/// 開始のフックは、その間 2 つの入れ物に触らない: 入力の入口は指図（`rollback_play_win::input_hook`）があれば feed して戻り、無ければ poll_all へ
/// 渡す。開始のフックは `MATCH_NESTED` に数えて戻る。
pub static RB_HELD: AtomicU32 = AtomicU32::new(0);
/// フックが入れ物を持っている間に来た開始のフック（来ない見込み）。
pub static MATCH_NESTED: AtomicU32 = AtomicU32::new(0);
/// フックが入れ物を持っている間に、指図が無いまま入力の入口へ来た回数（来ない見込み。来たら本物のキーボードを rollback の tick に入れないよう
/// poll_all を呼ばずに戻した）。
pub static RB_STRAY: AtomicU32 = AtomicU32::new(0);
/// 走査の包みのフックを通ったフレームの数（本ログ用の数え `rollback_play_win::publish_summary` を数フレームに 1 回にする）。
static RB_FRAMES: AtomicU32 = AtomicU32::new(0);
/// マッチをやめた回数（`NetPlay` の数えをフックがコピーする。増えたのを見たら、その場で本ログ用の数えもコピーする —— やめた後はフックが数えをコピーしなくなるので、
/// コピーしないと要約がやめる前の値のまま残る）。
static RB_ABORTS: AtomicU32 = AtomicU32::new(0);
/// 最初にマッチをやめた経路（`abort_code`）と、そのときまでに確定したフレームの数。
static RB_ABORT_FIRST: AtomicU32 = AtomicU32::new(0);
static RB_ABORT_FRAME: AtomicU32 = AtomicU32::new(0);
/// rollback のマッチの間に、フックが入れ物（`real_io` / `with_play`）を借りられず走査を普段どおりに回した回数（来ない見込み）。
static RB_BYPASS: AtomicU32 = AtomicU32::new(0);

/// マッチをやめたのを見たら（`NetPlay` の数えがコピーした値より多い）、経路をコピー、本ログ用の数えもその場でコピーする。
fn rb_note_abort(np: &netplay::NetPlay) {
    let n = np.rb_aborts();
    if n == RB_ABORTS.load(Ordering::Relaxed) {
        return;
    }
    if let Some((why, frame)) = np.rb_abort_first() {
        RB_ABORT_FRAME.store(frame.min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);
        RB_ABORT_FIRST.store(abort_code(why), Ordering::Relaxed);
    }
    RB_ABORTS.store(n, Ordering::Relaxed);
    crate::rollback_play_win::publish_summary(np.rollback_core_stats());
}
/// 台本が最後の段階まで押した後もネットワークを閉じない（ini の `after_script = keep`）。`open_net` が置き、ゲームのスレッドが `net_tick` で読む。
static KEEP_AFTER_SCRIPT: AtomicU32 = AtomicU32::new(0);

pub struct NetSlot {
    pub np: NetPlay,
    pub net: GameNet,
}

/// 観戦者のセッション（ini の `code_side = spectate`。対戦のセッション `NetSlot` の代わりに置く）。
pub struct WatchNetSlot {
    pub wp: WatchPlay,
    pub net: GameNet,
    pub shots: crate::spectate_watch::ShotPacer,
}

enum Slot {
    Play(Box<NetSlot>),
    Watch(Box<WatchNetSlot>),
}

struct NetCell(UnsafeCell<Option<Slot>>);
// SAFETY: 読み取りのスレッドの `arm_net` / `arm_watch` は `NET_ON` と `SCRIPT_ON` が 0 の間だけ置き（本番では 3 つのフックを書く前に 1 回だけ）、以後はゲームのスレッドだけが触る
// （入れ子にならない。開始のフックが別のスレッドから来たら触らない —— `GAME_TID`）。`start_net` は Release、ゲームのスレッドは Acquire で
// 読んでから触る。
unsafe impl Sync for NetCell {}
static NET: NetCell = NetCell(UnsafeCell::new(None));

// ---- ゲーム内のメニュー（ini の `code_side = game`）----
//
// - 起動のとき（`arm_game_ui`）: 受信ソケット・STUN・見張りは**開かない**。メニューの入れ物（`UI`）と、ネットワークを開くのに要るもの（`LAZY`）だけを置く。
//   入力の入口は開く（`UI_ON`）が、画面 6 でメニューが入力を握るだけで、描くものは無い。「妖怪 対 妖怪」が選ばれない限り、ゲームの動作に
//   何も影響しない。
// - 最初の「作る」「貼る」（`UiOut::open_net`）で、別のスレッドがネットワークを開く（`request_open` → `open_game_net`）。ゲームのスレッドは待たず、
//   開く間のメニューに「準備しています」を出す。開き終わったら、ゲームのスレッドが次の tick でネットワークのセッションを `NET` へ置いて `NET_ON` を立てる
//   （`poll_pending` → `install_slot`）。
// - つなぎ直し: つながりが終わり、`RELINK_GRACE` 経って場面がマッチの外にあれば、ゲームのスレッドはネットワークの経路の tick の終わりにネットワークのセッションを手放す
//   （`relink_teardown`。`NET_ON` を 0 にしてセッションを郵便受け `CLOSING` へ置き、メニューを「つなぐ前」へ戻すだけ。確保・待ち・スレッドの起こしは
//   無い）。ワーカーのスレッド（`higanr-relink`）が `relink_tick` でネットワークの組を片づける。次の「作る」「貼る」「ルームで待つ」で新しい組を開く。
//   マッチの中では片づけない。`offer` / `answer`・観戦・ini の相手の形はメニューを置かないので片づけない。

/// ゲーム内のメニューが動いているか。入力の入口の `route` が見る。0 → 1 にするのは読み取りのスレッド（`arm_game_ui`。メニューの入れ物を
/// 置いた後）だけ、1 → 0 にするのはゲームのスレッド（パニック）。
pub static UI_ON: AtomicU32 = AtomicU32::new(0);

struct UiCell(UnsafeCell<Option<NetUi>>);
// SAFETY: 読み取りのスレッドの `arm_game_ui` は `UI_ON` が 0 の間だけ置き（起動の 1 回。入力の入口の分岐を開く前）、以後はゲームのスレッドだけが `game_ui` を通して触る。
// `UI_ON` は Release で 1 にし、Acquire で読んでから触る。
unsafe impl Sync for UiCell {}
static UI: UiCell = UiCell(UnsafeCell::new(None));

struct LazyNet {
    settings: NetSettings,
    /// 接続コードの置き場（開発用の `clipboard = file` のときだけ使う）。ゲームのフォルダの外であることは置く前に確かめた。
    code_dir: Option<PathBuf>,
    /// 起動のときに読んだ ini のパス。メニューで言語を替えたとき、この ini の `[ui] lang` を書き換える。
    ini_path: Option<PathBuf>,
    link: Arc<UiLink>,
}
static LAZY: OnceLock<LazyNet> = OnceLock::new();

impl LazyNet {
    /// ネットワークを開くときの設定: 起動のときの設定に、メニューで決めたプレイヤー名（`UiLink::player_name`）があればそれを入れたもの
    /// （名前を変えたら次に開くネットワークから新しい名前で名乗る。開くスレッドだけが呼ぶ）。
    fn settings_now(&self) -> NetSettings {
        let mut s = self.settings.clone();
        if let Some(n) = self.link.player_name() {
            s.name = n.as_str().to_string();
        }
        s
    }
}

/// 名前を覚えるファイルを書く仕事が重ならないための鍵（続けて名前を決めても、一時ファイルを取り合わない）。
static NAME_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// メニューで名前を決めた（ゲームのスレッド）: 覚えるファイル（置き場の `player_name.txt`）へ別のスレッドが書く（ゲームのスレッドでは
/// ファイルに書かない。走るのはスレッドを 1 本起こすことだけ）。書くのは**そのとき入れ物にある最後に決めた名前**。置き場が無い・書けない
/// ときは本ログへ書く（名前は含めない）。
fn save_player_name(n: NameBuf) {
    let Some(lazy) = LAZY.get() else {
        return;
    };
    let Some(dir) = lazy.code_dir.clone() else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("higanr-name".into())
        .spawn(move || {
            if let Err(e) = write_player_name(&dir, &lazy.link, n) {
                game_net_problem(format!("名前: 覚えるファイルへ書けなかった（{e}）"));
            }
        });
    if let Err(e) = spawned {
        game_net_problem(format!("名前: 書くスレッドを起こせなかった（{e}）"));
    }
}

/// 覚えるファイルへ名前を書く（`save_player_name` が起こしたスレッドで走る）。
fn write_player_name(dir: &Path, link: &UiLink, n: NameBuf) -> Result<(), String> {
    let _g = NAME_WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let latest = link.player_name().unwrap_or(n);
    crate::player_name_file::write(dir, &latest)
}

/// ini の言語の行を書く仕事が重ならないための鍵（続けて替えても、読み → 書きと一時ファイルを取り合わない）。
static LANG_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// メニューで言語を替えた（ゲームのスレッド）。
fn save_ui_lang(l: Lang) {
    apply_ui_lang(l, spawn_write_ui_lang);
}

/// 今の言語を置き（原子変数 1 回の書き。見張り・ウィンドウの文を書くスレッドも読む）、ini へ書く仕事を頼む。頼み方は差し込める
/// （検査は記録するだけの関数を渡す）。
fn apply_ui_lang(l: Lang, ask_write: impl FnOnce()) {
    crate::ui_lang::set(l);
    ask_write();
}

/// ini の `[ui] lang` の書き換えを別のスレッドに頼む（ゲームのスレッドではファイルに触らない。走るのはスレッドを 1 本起こすことだけ）。
/// 書くのは**そのとき置いてある最後の言語**。書けない・ini の置き場が無いときは本ログへ書く。
fn spawn_write_ui_lang() {
    let Some(lazy) = LAZY.get() else {
        return;
    };
    let path = lazy.ini_path.clone();
    let spawned = std::thread::Builder::new()
        .name("higanr-lang".into())
        .spawn(move || {
            let _g = LANG_WRITE.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = crate::ui_lang::save_to_ini(path.as_deref(), crate::ui_lang::current())
            {
                game_net_problem(format!("言語: ini へ書けなかった（{e}）"));
            }
        });
    if let Err(e) = spawned {
        game_net_problem(format!("言語: 書くスレッドを起こせなかった（{e}）"));
    }
}

/// 観戦の「ランダムマッチ」の一覧の接続（メニューが一覧の画面を開いている間だけ持つ）。触るのは `feed_watch_list` だけ。
static WATCH_FEED: std::sync::Mutex<Option<crate::watch_feed::WatchFeed>> =
    std::sync::Mutex::new(None);

/// メニューが観戦の「ランダムマッチ」の一覧の画面に入った（`true`）・出た（`false`）。入るときに一覧の接続のスレッドを起こし、出るときに止める
/// （ゲームのスレッド。人が画面を操作した tick に 1 回。確保・待ちは起こすスレッドの側）。
fn feed_watch_list(on: bool) {
    let mut g = WATCH_FEED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut f) = g.take() {
        f.stop();
    }
    if !on {
        return;
    }
    let Some(lazy) = LAZY.get() else {
        return;
    };
    let Some(base) = lazy.settings.room_server.clone() else {
        lazy.link
            .publish_watch_list(higanr_core::watch_list::ListStatus::Failed, &[]);
        return;
    };
    match crate::watch_feed::WatchFeed::start(
        Arc::clone(&lazy.link),
        RoomOptions::product(base),
        higanr_core::update::BUILD_NUMBER,
    ) {
        Ok(f) => *g = Some(f),
        Err(e) => {
            game_net_problem(e);
            lazy.link
                .publish_watch_list(higanr_core::watch_list::ListStatus::Failed, &[]);
        }
    }
}

/// 最後に決めたルーム名の回の番号（古い方のスレッドが新しい名を上書きしないため）。取るのはゲームのスレッド、見るのは書くスレッドの
/// `room_last::write_if_latest` の鍵の中。
static ROOM_SAVE_SEQ: AtomicU32 = AtomicU32::new(0);

/// 設定の「ルーム名」を決めた（ゲームのスレッド）: 「前に使ったルーム名」（置き場の `room_last.txt`）へ別のスレッドが書く。ゲームのスレッドでは
/// ファイルに書かない。誤りの文は名を含めない。
fn save_room_name(n: RoomBuf) {
    let Some(lazy) = LAZY.get() else {
        return;
    };
    let Some(dir) = lazy.code_dir.clone() else {
        return;
    };
    let seq = ROOM_SAVE_SEQ.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    let spawned = std::thread::Builder::new()
        .name("higanr-room".into())
        .spawn(move || {
            if let Err(e) = write_room_name(&dir, seq, &n) {
                game_net_problem(format!("ルーム名: 覚えるファイルへ書けなかった（{e}）"));
            }
        });
    if let Err(e) = spawned {
        game_net_problem(format!("ルーム名: 書くスレッドを起こせなかった（{e}）"));
    }
}

/// 前に使ったルーム名を書く（`save_room_name` が起こしたスレッドで走る）。もっと新しく決めた回があれば何も書かない。番号を見るのも
/// 書くのも `room_last` の鍵の中。
fn write_room_name(dir: &Path, seq: u32, n: &RoomBuf) -> Result<(), String> {
    crate::room_last::write_if_latest(dir, n, || ROOM_SAVE_SEQ.load(Ordering::Acquire) == seq)
}

/// ネットワークの開き: 0 = まだ（か、開けなかった・片づけ終えたのでやり直せる）/ 1 = 別のスレッドが開いている最中 / 2 = ネットワークのセッションを置いた /
/// 3 = つながりが終わってゲームのスレッドがネットワークのセッションを手放し、ワーカーのスレッドが片づけている最中 / 4 = 片づけている最中に人が「作る」
/// 「貼る」「ルームで待つ」を選んだ（つなぎ直しのスレッドが片づけ終えたらそのまま開く）。
static OPEN_STATE: AtomicU32 = AtomicU32::new(0);
const OPEN_IDLE: u32 = 0;
const OPEN_BUSY: u32 = 1;
const OPEN_DONE: u32 = 2;
const OPEN_CLOSING: u32 = 3;
const OPEN_REOPEN: u32 = 4;

/// 開くスレッドが作ったネットワークのセッション（ゲームのスレッドが次の tick で取って置く）。対戦者のセッションか、観戦者のセッション（「ルームで観戦」）。
enum Pending {
    Play(Box<NetSlot>),
    Watch(Box<WatchNetSlot>),
}
// SAFETY: 開くスレッドが作ってここへ**渡す**（移す）だけで、渡した後は開くスレッドは触らない。取り出して使うのはゲームのスレッドだけ。
// ネットワークのセッションの中身（`NetPlay`・`WatchPlay`・`GameNet`）は、ゲームのスレッドから使われる前提で作ってあり、作ったスレッドに縛られる物
// （スレッドのローカル・`Rc`）を持たない（`open_net` は読み取りのスレッドで作って、ゲームのスレッドへ渡している）。
unsafe impl Send for Pending {}
static PENDING: std::sync::Mutex<Option<Result<Pending, ()>>> = std::sync::Mutex::new(None);
/// ゲームのスレッドがネットワークのセッションを置けなかった回数（準備が壊れているときだけ。理由の文はゲームのスレッドでは作らず、数だけ本ログへ出す）。
static INSTALL_FAILS: AtomicU32 = AtomicU32::new(0);
/// ネットワークを開く仕事を始めた回数（開くスレッドが数える。試験のまとめが「頼みが来たか」を見る）。
static OPEN_TRIES: AtomicU32 = AtomicU32::new(0);
/// ネットワークを開く仕事の本ログの文（`net_status` が「ネットワークの外」の行へ足す）。
static GAME_NET_NOTES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// 失敗の文。「ネットワークの外」の行（詳細）に加えて、簡潔なログにもその場で 1 行出す。
fn game_net_problem(line: String) {
    crate::main_log::push(line.clone());
    game_net_note(line);
}

fn game_net_note(line: String) {
    if let Ok(mut g) = GAME_NET_NOTES.lock() {
        if g.len() >= 32 {
            g.remove(0);
        }
        g.push(line);
    }
}

// ---- つなぎ直し ----

enum Released {
    Play(Box<NetSlot>),
    Watch(Box<WatchNetSlot>),
}

struct Closing {
    slot: Released,
    link: Arc<UiLink>,
    /// 手放した tick までに送られた指示の数（`UiLink::order_mark`）。これより前の指示は前の見張りの残りとして捨てる。
    mark: u32,
}
// SAFETY: ゲームのスレッドが置いた後はゲームのスレッドは触らず（`NET` からは取り出し済み・`NET_ON` は 0）、ワーカーのスレッドだけが取り出して
// 落とす。中身は `Pending` と同じく、作ったスレッドに縛られる物を持たない。
unsafe impl Send for Closing {}
/// 郵便受け（ゲームのスレッドは `try_lock` だけ。取れなければ次の tick に手放す）。
static CLOSING: std::sync::Mutex<Option<Closing>> = std::sync::Mutex::new(None);
/// 1 = 手放したネットワークのセッションを片づけたら、続けて観戦者のネットワークを開く。ゲームのスレッドが書き、ワーカーのスレッドが読んで 0 に戻す。「作る」「貼る」
/// 「ルームで待つ」が後から選ばれたら 0 に戻す（最後に選んだ方が勝つ）。
static SPECTATE_AFTER: AtomicU32 = AtomicU32::new(0);
/// ワーカーのスレッドを起こしたか（1 = 起こした。0 の実行では手放さない —— 片づける者が居ないので、今までどおりネットワークは残る）。
static RELINK_STARTED: AtomicU32 = AtomicU32::new(0);
/// ネットワークが終わってから手放すまでの待ち（ms。`netplay::RELINK_GRACE_MS`。検査は短くする）。
static RELINK_GRACE: AtomicU32 = AtomicU32::new(netplay::RELINK_GRACE_MS);
/// ワーカーのスレッドが郵便受けを見に行く間隔。片づけの後の「作る」が開き始めるまでの遅れの上限で、マッチの外の人の操作に対して気づかない長さ。
const RELINK_POLL: std::time::Duration = std::time::Duration::from_millis(20);
static LINKS_CLOSED: AtomicU32 = AtomicU32::new(0);
static PAST_LINKS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
/// 本ログに残す片づけたつながりの数の上限（古い方から捨てる。1 つ数 KB）。
const PAST_LINKS_MAX: usize = 16;
/// 片づけた観戦者のセッションの数と、その節（本ログの「ネットワークの外」の終わりに足す。観戦者の集計 `WATCH_STATUS` は次の観戦で上書きされる）。
static WATCHES_CLOSED: AtomicU32 = AtomicU32::new(0);
static PAST_WATCHES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
/// 片づけた観戦の節のうち、まだ本ログの途中に書いていないもの（`take_watch_ends` が取り出して空にする）。観戦者はゲーム内のメニューの
/// 実行で計測が締まらないので、片づけた次の 1 秒の見回りで書く。上限は `PAST_LINKS_MAX`。
static UNLOGGED_WATCHES: std::sync::Mutex<Vec<(u32, String)>> = std::sync::Mutex::new(Vec::new());
/// 今 `CODE_WATCH` に残っている見張りの文は、片づけたときに `PAST_LINKS` へコピー済み（二重に出さない）。
static CODE_NOTES_ARCHIVED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// つながった（ことがある）接続の session（ゲームのスレッドが手放すときに書き、開くスレッドが `GameUi::spent` へコピーする）。
const SPENT_MAX: usize = 16;
static SPENT: [AtomicU32; SPENT_MAX] = [const { AtomicU32::new(0) }; SPENT_MAX];
static SPENT_N: AtomicU32 = AtomicU32::new(0);

fn spend(session: u32) {
    let n = SPENT_N.load(Ordering::Relaxed);
    SPENT[n as usize % SPENT_MAX].store(session, Ordering::Relaxed);
    SPENT_N.store(n.wrapping_add(1), Ordering::Release);
}

fn spent_sessions() -> Vec<u32> {
    let n = (SPENT_N.load(Ordering::Acquire) as usize).min(SPENT_MAX);
    SPENT[..n]
        .iter()
        .map(|a| a.load(Ordering::Relaxed))
        .collect()
}

/// ネットワークのセッションを手放す（ゲームのスレッド。`route` がネットワークの経路の tick の後に、`NetPlay::relink_due` が真のときだけ呼ぶ）。対戦者のセッションだけ。
/// 確保しない（セッションは `Box` のまま郵便受けへ移す）・待たない（`try_lock`。取れなければ次の tick）・スレッドを起こさない。手放した後は
/// 次の tick からメニューだけの経路（`ui_only_tick`）。
fn relink_teardown() {
    release_slot(|s| matches!(s, Slot::Play(_)));
}

/// 観戦者のセッションを手放す（ゲームのスレッド。観戦者のセッションが終わり（`Ended`）、観戦者のルームのスレッドが最後の状態を書き終えたとき）。
/// メニューは元の項目に戻る。
fn watch_teardown() {
    release_slot(|s| matches!(s, Slot::Watch(w) if w.wp.is_ended() && spectate_join::finished()));
}

/// ネットワークのセッション（対戦者・観戦者）を手放す共通の部分（ゲームのスレッド）。`allow` がセッションの状態を見て真のときだけ、セッションを郵便受けへ移す。
/// 手放せたら true。確保しない・待たない（`relink_teardown` の規律）。
fn release_slot(allow: impl FnOnce(&Slot) -> bool) -> bool {
    if RELINK_STARTED.load(Ordering::Acquire) == 0
        || OPEN_STATE.load(Ordering::Acquire) != OPEN_DONE
    {
        return false;
    }
    let Some(ui) = game_ui() else {
        return false;
    };
    let Ok(mut mailbox) = CLOSING.try_lock() else {
        return false;
    };
    if mailbox.is_some() {
        return false;
    }
    // SAFETY: ゲームのスレッド（`route` の中。ネットワークのセッションの借用は終わっている）。ネットワークのセッションに触るのはゲームのスレッドだけ（`NetCell` の Sync の注）。
    let cell = unsafe { &mut *NET.0.get() };
    if !cell.as_ref().is_some_and(allow) {
        return false;
    }
    // 先にネットワークの経路を閉じる（開始のフック・rollback のフックは `NET_ON` を見てセッションに触らない。どれも同じゲームのスレッド）。
    NET_ON.store(0, Ordering::SeqCst);
    let released = match cell.take() {
        Some(Slot::Play(slot)) => {
            if let Some(s) = slot.np.spent_session() {
                spend(s);
            }
            // 片づける直前のつながりの状態（終わりの状態なので、成立の前の残りの時計は使わない）。
            let (last, _) = slot.np.ui_conn(0);
            ui.after_teardown(last);
            Released::Play(slot)
        }
        Some(Slot::Watch(w)) => {
            ui.after_watch_teardown();
            Released::Watch(w)
        }
        None => return false,
    };
    publish_ui_stats(ui);
    let link = Arc::clone(ui.link());
    let mark = link.order_mark();
    *mailbox = Some(Closing {
        slot: released,
        link,
        mark,
    });
    OPEN_STATE.store(OPEN_CLOSING, Ordering::SeqCst);
    true
}

/// ワーカーのスレッドの 1 回（`RELINK_POLL` ごと）。片づけている間に人が選んでいたら、最初のネットワークと同じ仕事（`open_game_net`）で開く。
fn relink_tick() {
    relink_tick_with(|| {
        // 「ルームで観戦」が決まって手放した（か、片づけている間に選ばれた）なら観戦者のネットワーク、そうでなければ対戦者のネットワーク。予定は開く前に消す
        // （開けなくても次の片づけへ持ち越さない）。
        let watch = SPECTATE_AFTER.swap(0, Ordering::SeqCst) != 0;
        LAZY.get().map(|lazy| {
            if watch {
                open_watch_job(|| open_watch_net(lazy))
            } else {
                open_job(|| open_game_net(lazy))
            }
        })
    });
}

/// `relink_tick` の中身（検査は開く仕事を偽物にして直に呼ぶ）: 手放されたネットワークのセッションがあれば、前のつながりのネットワークの行を書き留めてからネットワークの組を片づけ、
/// 片づけている間に人が選んでいた（`OPEN_REOPEN`）なら `reopen` で新しい組を開いてゲームのスレッドへ渡す（`PENDING`）。
fn relink_tick_with(reopen: impl FnOnce() -> Option<Result<Pending, ()>>) {
    let st = OPEN_STATE.load(Ordering::Acquire);
    if st != OPEN_CLOSING && st != OPEN_REOPEN {
        return;
    }
    let Some(Closing { slot, link, mark }) =
        CLOSING.lock().unwrap_or_else(|e| e.into_inner()).take()
    else {
        return;
    };
    match slot {
        Released::Play(slot) => close_play(*slot, &link, mark),
        Released::Watch(w) => close_watch(*w, &link, mark),
    }
    // 手放した後に開くもの: 「ルームで観戦」が決まっていたか、片づけている間に何かが選ばれていた（`OPEN_REOPEN`）ときは開く。
    // 何も選ばれていなければ開き直せる形（`OPEN_IDLE`）に戻す。
    if SPECTATE_AFTER.load(Ordering::SeqCst) == 0
        && OPEN_STATE
            .compare_exchange(OPEN_CLOSING, OPEN_IDLE, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        return;
    }
    OPEN_STATE.store(OPEN_BUSY, Ordering::SeqCst);
    match reopen() {
        Some(r) => *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(r),
        None => OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst),
    }
}

/// 手放した対戦者のセッションを片づける（ワーカーのスレッド）: 前のつながりのネットワークの行を本ログ用に控えてから、ネットワークの組（見張り・受信スレッド・受信ソケット）を
/// 止めて閉じる。
fn close_play(slot: NetSlot, link: &Arc<UiLink>, mark: u32) {
    let n = LINKS_CLOSED.fetch_add(1, Ordering::SeqCst) + 1;
    // ゲームのスレッドはもうネットワークの数をコピーしない（`NET_ON` が 0）ので、ここで読む値が前のつながりの最後の値。
    let mut text = format!(
        "\n==== {n} 回目のつながり（片づけた。この節の数はこのつながりのぶん）====\nネットワーク: {}\n{}\n{}{}",
        NET_STATUS.snapshot().text(),
        NET_STATUS.menu_text(),
        NET_STATUS.timing_text(),
        rollback_text()
    );
    let NetSlot { np, net } = slot;
    let recv = RECV.lock().unwrap_or_else(|e| e.into_inner()).take();
    {
        let mut watch = CODE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
        match (watch.as_mut(), recv) {
            (Some(w), Some(r)) => {
                let stats = net_code_game::close_parts(np, net, w, r, link, mark);
                text += &format!("\n受信 {stats:?}");
                for l in w.notes() {
                    text += &format!("\n{l}");
                }
            }
            (w, r) => {
                // 来ない見込み（ゲーム内のメニューの実行は、開くときに見張りと受信スレッドを必ず置く）。在るものだけ止める。
                drop((np, net));
                if let Some(w) = w {
                    w.stop();
                }
                drop(r);
                link.reset_for_next_watcher(mark);
                text += "\n（片づけ: 見張りか受信スレッドが無かった）";
            }
        }
    }
    CODE_NOTES_ARCHIVED.store(true, Ordering::SeqCst);
    {
        let mut past = PAST_LINKS.lock().unwrap_or_else(|e| e.into_inner());
        if past.len() >= PAST_LINKS_MAX {
            past.remove(0);
        }
        past.push(text);
    }
    game_net_note(format!(
        "ネットワーク: {n} 回目のつながりを片づけた（見張り・受信スレッドを止め、受信ソケットを閉じた。メニューは「つなぐ前」）"
    ));
}

/// 手放した観戦者のセッションを片づける（ワーカーのスレッド）: 観戦者のルームのスレッドと受信スレッドを止めて受信ソケットを閉じ、観戦者の集計の行を書き留める。
/// 指示のリングバッファに残った指示は捨てる。状態は消さない（観戦の終わりの理由を左上に残す）。節には観戦者のルームの数の行も入れ、読み取りのスレッドが
/// 次の 1 秒の見回りで本ログの途中に書く（`take_watch_ends`）。
fn close_watch(w: WatchNetSlot, link: &Arc<UiLink>, mark: u32) {
    let WatchNetSlot { wp, net, .. } = w;
    // 最後の数を書き留める（セッションを落とす前に。`WATCH_STATUS` はゲームのスレッドが最後にコピーした値）。
    let watch_text = WATCH_STATUS.text();
    drop(wp);
    drop(net);
    // ルームの数の行はこの節にだけ入れる（`spectate_join::stats_text` は動いている分だけ。書き留める場所が 2 つあると全文で重なる）。
    let join_line = spectate_join::stop_and_join();
    let recv = RECV.lock().unwrap_or_else(|e| e.into_inner()).take();
    let stats = recv.map(RecvThread::stop);
    link.drop_orders_before(mark);
    let n = WATCHES_CLOSED.fetch_add(1, Ordering::SeqCst) + 1;
    let body = watch_section_body(join_line.as_deref(), &watch_text, &format!("{stats:?}"));
    {
        let mut past = PAST_WATCHES.lock().unwrap_or_else(|e| e.into_inner());
        if past.len() >= PAST_LINKS_MAX {
            past.remove(0);
        }
        past.push(format!(
            "\n==== {n} 回目の観戦（片づけた。この節の数はこの観戦のぶん）====\n{body}"
        ));
    }
    {
        let mut unlogged = UNLOGGED_WATCHES.lock().unwrap_or_else(|e| e.into_inner());
        if unlogged.len() >= PAST_LINKS_MAX {
            unlogged.remove(0);
        }
        unlogged.push((n, body));
    }
    game_net_note(format!(
        "観戦: {n} 回目の観戦者のセッションを片づけた（ルームのスレッド・受信スレッドを止め、受信ソケットを閉じた。メニューは「つなぐ前」）"
    ));
}

/// 片づけた観戦の節の中身（見出しは呼び手が付ける）: ルームの数の行・観戦者の集計（無ければ飛ばす）・受信の数。
fn watch_section_body(join_line: Option<&str>, watch_text: &str, recv: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(l) = join_line {
        lines.push(l.to_string());
    }
    if !watch_text.is_empty() {
        lines.push(watch_text.to_string());
    }
    lines.push(format!("受信 {recv}"));
    lines.join("\n")
}

/// 片づけた観戦の節のうち、まだ本ログの途中に書いていないものを取り出す（読み取りのスレッド。取り出したら空になるので 1 つの節は
/// 1 回だけ書かれる）。番号は `close_watch` の「n 回目の観戦」。
pub fn take_watch_ends() -> Vec<(u32, String)> {
    std::mem::take(&mut *UNLOGGED_WATCHES.lock().unwrap_or_else(|e| e.into_inner()))
}

/// 観戦者のセッションがある間（ルームのスレッドが動いている・観戦中）の本ログの途中経過の文（読み取りのスレッド）: いまのルームの数の行と、
/// 観戦者の集計（あれば）。観戦者のセッションが無い実行は `None`。
pub fn watch_live_text() -> Option<String> {
    let join = spectate_join::active_line()?;
    Some(if WATCH_STATUS.published() {
        format!("{join}\n{}", WATCH_STATUS.text())
    } else {
        join
    })
}

/// ワーカーのスレッドを起こす（開くスレッドから。実行で 1 回だけ）。起こせなければ手放さない（ネットワークはつながりが終わっても残り、起動し直すまで
/// 開き直せない）。
fn start_relink_thread() {
    if RELINK_STARTED.swap(1, Ordering::SeqCst) != 0 {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("higanr-relink".into())
        .spawn(|| {
            loop {
                std::thread::sleep(RELINK_POLL);
                // パニックは受けて外へ出さない（スレッドの入口）。片づけの途中でパニックしたら、開き直せる形に戻す。
                if catch_unwind(relink_tick).is_err() {
                    game_net_problem(
                        "ネットワーク: 片づけのワーカーでパニックした（開き直せる形に戻した）"
                            .to_string(),
                    );
                    OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
                }
            }
        });
    if let Err(e) = spawned {
        RELINK_STARTED.store(0, Ordering::SeqCst);
        game_net_problem(format!(
            "ネットワーク: 片づけのワーカーのスレッドを起こせない（{e}）。つながりが終わった後は起動し直すまで開き直せない"
        ));
    }
}

/// ゲーム内のメニューを置く（読み取りのスレッド。`SCRIPT_ON` と `NET_ON` が 0 の間だけ）。ネットワークは開かない。メニューのセッションと、ネットワークを開くのに
/// 要るものを置き、`arm_net` と同じ準備の確かめをする。
pub fn arm_game_ui(
    s: &NetSettings,
    set_state_fn: u32,
    code_dir: Option<PathBuf>,
    ini_path: Option<PathBuf>,
) -> Result<String, String> {
    arm_game_ui_with(
        s,
        set_state_fn,
        code_dir,
        ini_path,
        crate::ui_lang::current(),
        &net_game::direct_ranges(),
    )
}

/// `arm_game_ui` の中身（メニューが始まる言語と、読めるかを見る範囲を渡す。検査は自分の言語と静的変数を渡す）。
fn arm_game_ui_with(
    s: &NetSettings,
    set_state_fn: u32,
    code_dir: Option<PathBuf>,
    ini_path: Option<PathBuf>,
    lang: Lang,
    ranges: &[(u32, u32)],
) -> Result<String, String> {
    if SCRIPT_ON.load(Ordering::SeqCst) != 0 || NET_ON.load(Ordering::SeqCst) != 0 {
        return Err("入口の分岐がもう開いている".to_string());
    }
    check_ready(set_state_fn, ranges)?;
    if UI_ON.load(Ordering::SeqCst) != 0 || LAZY.get().is_some() {
        return Err("ゲーム内のメニューはもう置いてある".to_string());
    }
    SET_STATE_FN.store(set_state_fn, Ordering::SeqCst);
    GAME_TID.store(0, Ordering::SeqCst);
    let link = Arc::new(UiLink::new());
    let (ini, start_auto) = menu_start(s);
    // ルーム名の入力の初めの値: ini の `room`（決まりを通るもの）→ 前に使った名（置き場の `room_last.txt`）→ 空。読むのはここ（起動の
    // ときの読み取りのスレッド）だけで、ゲームのスレッドは読まない。
    let last = code_dir.as_deref().and_then(crate::room_last::read);
    let (initial, from) =
        higanr_core::room_input::initial_from(s.room.as_ref().map(|r| r.0.as_str()), last);
    LAZY.set(LazyNet {
        settings: s.clone(),
        code_dir,
        ini_path,
        link: Arc::clone(&link),
    })
    .map_err(|_| "ゲーム内のメニューはもう置いてある".to_string())?;
    let room = room_item(s, initial);
    // 「名前」の項目: 今の名前（打てない文字は `?` に直した表示用の形）。決めたら覚えるファイルへ別のスレッドが書く。
    // 「サイド」の初めの値は「おまかせ」（ini に `host_seat` があればその席）。
    let ui = NetUi::new(ini, link, &BOARD)
        .with_start_auto(start_auto)
        .with_room(room)
        .with_seat(initial_seat(s))
        .with_player_name(NameBuf::lossy(&s.name), Some(save_player_name))
        .with_room_saver(Some(save_room_name))
        .with_watch_feed(Some(feed_watch_list))
        .with_lang(lang, Some(save_ui_lang));
    // SAFETY: `UI_ON` が 0 なので、ゲームのスレッドはメニューの入れ物に触らない（`UiCell` の Sync の注）。
    unsafe { *UI.0.get() = Some(ui) };
    UI_ON.store(1, Ordering::Release);
    Ok(format!(
        "ゲーム内のメニューを置いた（受信ソケット・STUN・見張りは、メニューの最初の「ルームで待つ」「作る」「貼る」まで開かない。コードの受け渡しは{}。ルームで待つは{}）",
        match s.clipboard {
            ClipboardKind::System => "クリップボード",
            ClipboardKind::File => "ファイル（開発用 clipboard = file）",
        },
        match (room, from) {
            (RoomItem::Off, _) => "出さない（room_server が無い）",
            // 名は書かない。初めの値の出どころだけ。
            (RoomItem::On(_), InitialFrom::Ini) => {
                "出す（ルーム名はメニューで打つ。初めの値は ini の room。名は記録しない）"
            }
            (RoomItem::On(_), InitialFrom::LastName) => {
                "出す（ルーム名はメニューで打つ。初めの値は前に使った名。名は記録しない）"
            }
            (RoomItem::On(_), InitialFrom::Empty) => {
                "出す（ルーム名はメニューで打つ。初めの値は空。名は記録しない）"
            }
        }
    ))
}

/// 「ルームで待つ」の項目（ini の `room_server` が無ければ出さない。あれば入力の初めの値 `initial` を持たせる）。
pub(crate) fn room_item(s: &NetSettings, initial: higanr_core::room_input::RoomBuf) -> RoomItem {
    match &s.room_server {
        None => RoomItem::Off,
        Some(_) => RoomItem::On(initial),
    }
}

/// メニューの「同期方式」「ディレイ」の初めの値（ini の形と、ディレイが auto か）。ディレイは ini の `delay`（auto か、どちらの
/// 形式でも同じ数）。auto のときの D は rollback の既定（メニューで rollback へ替えて auto のままなら AGREE の D になる）。
pub(crate) fn menu_start(s: &NetSettings) -> (Shape, bool) {
    let auto = match s.mode {
        higanr_core::wire::Mode::Rollback => s.rollback_delay_auto,
        higanr_core::wire::Mode::Delay => s.latency_auto,
    };
    let shape = Shape {
        mode: s.mode,
        delay: s.rollback_delay,
    };
    (shape, auto)
}

/// メニューの「サイド」の初めの値: ini に `host_seat` を書いてあればその席、無ければ「おまかせ」。
pub(crate) fn initial_seat(s: &NetSettings) -> higanr_core::wire::SeatWish {
    if s.host_seat_given {
        higanr_core::seat_pick::SeatAsk::plain(s.host_seat).wish
    } else {
        higanr_core::wire::SeatWish::Any
    }
}

/// メニューの数え（`UiStats` のコピー。ゲームのスレッドが書き、読み取りのスレッドが本ログへ読む）。
static UI_STATS: [AtomicU32; 7] = [const { AtomicU32::new(0) }; 7];

/// 本ログの「ネットワークの外」の行へ足すメニューの数え（メニューを置いていない実行は空）。
fn ui_stats_text() -> String {
    if LAZY.get().is_none() {
        return String::new();
    }
    let l = |i: usize| UI_STATS[i].load(Ordering::Relaxed);
    format!(
        "\nゲーム内のメニュー: 画面 6・7 で入力を握った {} tick / メニューを開いた {} 回（入口が決定を奪った {} 回）/ 見張りへの指示 {} 個 / 自動の前進の押下 {} 回 / 確かめの枠 {} 回（はい {} 回）/ ゲームのスレッドがネットワークのセッションを置けなかった {} 回\nルーム名の入力: {} / 台本の文字を入れた {} 単位・入力欄が閉じていて捨てた {} 単位（文字そのものは記録しない）",
        l(0),
        l(1),
        l(2),
        l(3),
        l(4),
        l(5),
        l(6),
        INSTALL_FAILS.load(Ordering::Relaxed),
        crate::text_input_win::install_text(),
        TYPED_PUSHED.load(Ordering::Relaxed),
        TYPED_DROPPED.load(Ordering::Relaxed)
    )
}

/// 台本の `type=` / `bs=` / `enter=` の文字を、ルーム名の入力欄へ入れた・入力欄が閉じていて捨てた単位の数（ゲームのスレッドが書き、本ログが読む。
/// 実機の回で台本の文字が届いたかを見る）。
static TYPED_PUSHED: AtomicU32 = AtomicU32::new(0);
static TYPED_DROPPED: AtomicU32 = AtomicU32::new(0);

/// ゲーム内のメニューが置かれていて、ネットワークをまだ開いていないか（計測が「ネットワークが終わるのを待つ」判定で使う）。
pub fn net_pending() -> bool {
    UI_ON.load(Ordering::SeqCst) != 0 && NET_ARMED.load(Ordering::SeqCst) == 0
}

/// ネットワークを開く頼み（ゲームのスレッド）。別のスレッドに開かせる（受信ソケット・STUN の問い合わせは待つので）。すでに開いている最中・開いた後は
/// 何もしない。ゲームのスレッドで走るのはスレッドを 1 本起こすことだけで、人が「作る」「貼る」を決めた tick に 1 回。その tick はマッチも
/// lockstep も動いていない（ネットワークのセッションがまだ無い）ので遅延に効かない。起動のときに待たせておく案は「選ばれるまで何も起こさない」に反する
/// ので採らない。
fn request_open() {
    // 前のつながりをつなぎ直しのスレッドが片づけている最中（`OPEN_CLOSING`）なら、片づけ終えたスレッドがそのまま開く（`OPEN_REOPEN`。ここではスレッドを
    // 起こさない）。片づけ終えた瞬間と重なったら、もう一度見て `OPEN_IDLE` からの経路へ行く（3 回まで）。
    let mut won = false;
    for _ in 0..3 {
        match OPEN_STATE.load(Ordering::SeqCst) {
            OPEN_IDLE => {
                if OPEN_STATE
                    .compare_exchange(OPEN_IDLE, OPEN_BUSY, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    won = true;
                    break;
                }
            }
            OPEN_CLOSING => {
                // 片づけの後に観戦者のネットワークを開く予定だったなら、最後に選んだ「作る」「貼る」「ルームで待つ」が勝つ。
                SPECTATE_AFTER.store(0, Ordering::SeqCst);
                if OPEN_STATE
                    .compare_exchange(
                        OPEN_CLOSING,
                        OPEN_REOPEN,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok()
                {
                    return;
                }
            }
            _ => return,
        }
    }
    if !won {
        return;
    }
    // 片づけの最中に観戦の予定だけが残った（片づけ終えた瞬間と重なった）なら消す（次の片づけで観戦者のネットワークを開いてしまう）。
    SPECTATE_AFTER.store(0, Ordering::SeqCst);
    let Some(lazy) = LAZY.get() else {
        OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
        return;
    };
    spawn_open(lazy, false);
}

/// ネットワーク（対戦者のネットワークか観戦者のネットワーク）を開くスレッドを起こす（ゲームのスレッド。`OPEN_BUSY` に変えた後に 1 回だけ呼ぶ）。起こせなければ
/// 本ログに書き、メニューへ「内部の不具合」を知らせて、やり直せる形（`OPEN_IDLE`）に戻す。
fn spawn_open(lazy: &'static LazyNet, watch: bool) {
    let spawned = std::thread::Builder::new()
        .name("higanr-open".into())
        .spawn(move || {
            let r = if watch {
                open_watch_job(|| open_watch_net(lazy))
            } else {
                open_job(|| open_game_net(lazy))
            };
            let mut g = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            *g = Some(r);
        });
    if let Err(e) = spawned {
        game_net_problem(format!("ネットワークを開くスレッドを起こせない: {e}"));
        let n = Note {
            event: Event::Internal,
            seq: 1,
            ..Note::default()
        };
        lazy.link.publish(&n);
        OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
    }
}

/// 「ルームで観戦」が決まった（ゲームのスレッド）。**対戦者のネットワークは開かない。** 状態ごとの扱い:
/// - `OPEN_IDLE`: 開くスレッドを起こす。`OPEN_CLOSING`: 片づけ終えたワーカーのスレッドが続けて開く（`SPECTATE_AFTER`）。
/// - `OPEN_DONE`: つながっていないセッション（待っているだけの対戦者のセッション・終わった観戦者のセッション）は手放して、続けて開く。使っているセッションは手放さず、
///   指示を読み捨てる（来ない見込み）。
/// - `OPEN_BUSY` / `OPEN_REOPEN`: 読み捨てる。
fn request_spectate() {
    SPECTATE_REQUESTS.fetch_add(1, Ordering::Relaxed);
    for _ in 0..3 {
        match OPEN_STATE.load(Ordering::SeqCst) {
            OPEN_IDLE => {
                if OPEN_STATE
                    .compare_exchange(OPEN_IDLE, OPEN_BUSY, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    // 片づけ終えた瞬間と重なった（`OPEN_CLOSING` の枝で予定を置いた後に `OPEN_IDLE` へ戻された）なら、予定は要らない
                    // （ここで直に開く）。残すと次の片づけで 2 本目を開く。
                    SPECTATE_AFTER.store(0, Ordering::SeqCst);
                    match LAZY.get() {
                        Some(lazy) => spawn_open(lazy, true),
                        None => OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst),
                    }
                    return;
                }
            }
            OPEN_CLOSING => {
                SPECTATE_AFTER.store(1, Ordering::SeqCst);
                if OPEN_STATE
                    .compare_exchange(
                        OPEN_CLOSING,
                        OPEN_REOPEN,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok()
                {
                    return;
                }
            }
            OPEN_DONE => {
                SPECTATE_AFTER.store(1, Ordering::SeqCst);
                let released = release_slot(|s| match s {
                    // 対戦者のセッション: まだ誰ともつながっていない（待っているだけ。メニューが「ルームで観戦」を出すのはこのときだけ）。
                    Slot::Play(p) => matches!(p.np.ui_conn(0).0, Conn::Waiting),
                    // 観戦者のセッション: 終わっていて、ルームのスレッドが最後の状態を書き終えた。
                    Slot::Watch(w) => w.wp.is_ended() && spectate_join::finished(),
                });
                if !released {
                    SPECTATE_AFTER.store(0, Ordering::SeqCst);
                    SPECTATE_REFUSED.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
            _ => return,
        }
    }
}

/// 「ルームで観戦」の指示を断った回数（セッションが使われていた・手放せなかった。来ない見込み。本ログ用）。
static SPECTATE_REFUSED: AtomicU32 = AtomicU32::new(0);
/// 「ルームで観戦」の頼みを受けた回数（`request_spectate` を呼んだ数。本ログ用）。
static SPECTATE_REQUESTS: AtomicU32 = AtomicU32::new(0);

/// 検査だけ: 1 にすると、次に `UiOut` を見る場所（`spectate_asked`）が「ルームで観戦が決まった」と読んで 0 に戻す（ウィンドウから文字を打てない検査から
/// 観戦の決定を作るため）。
#[cfg(test)]
static FORCE_SPECTATE_OPEN: AtomicU32 = AtomicU32::new(0);

/// この tick のメニューの結果が「ルームで観戦」の決定か。
fn spectate_asked(out: &UiOut) -> bool {
    #[cfg(test)]
    if FORCE_SPECTATE_OPEN.swap(0, Ordering::SeqCst) != 0 {
        return true;
    }
    out.spectate_open
}

/// 起動のときに rollback の入口を書くかを決める「望み」。**対戦者は望みによらず `Rollback`**（ini が delay 方式でも、キャラセレクトの Tab で
/// rollback へ替えられるように）。観戦者はマッチを動かさないので、ini が delay 方式の実行は 1 バイトも書かない（走査の call だけを
/// `arm_watch_scan` が置く）。
fn install_wish(s: &NetSettings) -> higanr_core::wire::Mode {
    if s.code_side == Some(CodeSide::Spectate) {
        s.mode_wish().mode
    } else {
        higanr_core::wire::Mode::Rollback
    }
}

/// マッチの経路へ渡す `netplay::Config`（`open_net` と `open_game_net` が同じ値で作る）。
fn play_config(s: &NetSettings, rb_wired: bool) -> netplay::Config {
    netplay::Config {
        check: s.check,
        wait_budget_us: s.wait_budget_us,
        lat_plan: s.lat_plan,
        // ネットワークの選択を通らない場面 2（タイトルのデモ・リプレイの再生など）では止める地点に入らない（`netplay::Config::require_net_selection`）。
        require_net_selection: true,
        // HELLO で名乗る望むモード。ini が rollback でも、rollback のマッチの経路の入口を全部書けていない実行は delay 方式を名乗る
        // （書けなかったのに名乗ると、相手は予測で進めるのにこちらは待つ）。
        mode_wish: s.mode_wish_if(rb_wired),
        rollback_ahead: s.rollback_ahead,
        // rollback が「できる」か（HELLO の +3 の bit1。望みとは別）。入口を全部書けていれば、delay 方式を望んでいても、両者ができる組は
        // キャラセレクトの Tab で rollback へ替えられる。
        rollback_able: rb_wired,
        // 対戦の成立を READY で待つのは、ゲーム内のメニューの実行だけ（両者が「妖怪 対 妖怪」を選んだら成立。`match_gate`）。
        // `offer` / `answer` は繋がったときに成立する。
        match_gate: s.code_side == Some(CodeSide::Game),
        // 遅れの量をホストが決める（`delay_pick`）のも、ゲーム内のメニューの実行だけ。ini の `lat_plan` を書いた実行は予定のほうを使う。
        delay_pick: s.code_side == Some(CodeSide::Game) && s.lat_plan.is_empty(),
        // フレームの始まりで相手の入力ワードを待つ（ini の `head_wait`。既定 1）。
        head_wait_cap_us: if s.head_wait {
            netplay::HEAD_WAIT_CAP_US
        } else {
            0
        },
    }
}

/// ネットワークを開く（開くスレッド。`code_side = game`）: 受信ソケット・STUN・見張り・ネットワークのセッション。`open_net` の接続コードの形と同じ手順で、置き場へは触らない
/// （`clipboard = file` のときだけ置き場の下のファイルを使う）。戻りは置くネットワークのセッション。
fn open_game_net(lazy: &'static LazyNet) -> Result<Box<NetSlot>, String> {
    let named = lazy.settings_now();
    let s = &named;
    // 役割の乱数は開くたびに引く（ini に書いてあればその値。つなぎ直しでも同じ値）。
    let nonce = s.role_nonce.unwrap_or_else(default_nonce);
    let rb_wired = crate::rollback_play_win::armed();
    let cfg = play_config(s, rb_wired);
    let clip: Box<dyn Clipboard> = match (s.clipboard, lazy.code_dir.as_deref()) {
        (ClipboardKind::System, _) => Box::new(SystemClipboard),
        (ClipboardKind::File, Some(d)) => {
            std::fs::create_dir_all(d)
                .map_err(|e| format!("置き場 {} を作れない: {e}", d.display()))?;
            Box::new(FileClipboard::new(d.to_path_buf()))
        }
        (ClipboardKind::File, None) => return Err("接続コードの置き場が決まっていない".to_string()),
    };
    let net_code_game::Opened {
        np,
        net,
        watch,
        recv,
        local: local_addr,
        timer,
        note,
    } = net_code_game::open_set(
        s,
        cfg,
        nonce,
        net_code::Timing::default(),
        crate::notify_win::connected,
        // メニューで選んだ形を、自分のプロセス番号と関係なく採る（ファイルを経由しない）ので `pid` は照合に使わない。
        net_code::ModeCtx::new(s, std::process::id(), rb_wired),
        GameUi {
            link: Arc::clone(&lazy.link),
            clip,
            // ルームサーバー: OS のプロキシの設定に従う製品の待ち。
            room: s.room_server.clone().map(RoomOptions::product),
            // 前に使ったルーム名を覚えるファイルの置き場（接続コードの置き場。ゲームのフォルダの外 —— `arm_net` が確かめてある）。
            room_last_dir: lazy.code_dir.clone(),
            // つなぎ直し: 前のつながりで使ったコードは読まない。
            spent: spent_sessions(),
        },
    )?;
    let local = local_addr.map_or("（分からない）".to_string(), |a| a.to_string());
    let stun = watch.facts().stun;
    // 前のつながりの見張り（止めてある。文は片づけたときに本ログへコピーしてある）と置き換える。
    *CODE_WATCH.lock().unwrap_or_else(|e| e.into_inner()) = Some(watch);
    CODE_NOTES_ARCHIVED.store(false, Ordering::SeqCst);
    *OPEN_INFO.lock().unwrap_or_else(|e| e.into_inner()) = Some(OpenInfo {
        local: local_addr,
        scope: s.scope,
        stun,
    });
    KEEP_AFTER_SCRIPT.store(
        u32::from(s.after_script == AfterScript::Keep),
        Ordering::SeqCst,
    );
    *RECV.lock().unwrap_or_else(|e| e.into_inner()) = Some(recv);
    start_relink_thread();
    // 1 回目の行の文字は今までどおり。
    let nth = LINKS_CLOSED.load(Ordering::SeqCst) + 1;
    let how = if nth == 1 {
        "最初の操作で開いた".to_string()
    } else {
        format!("操作で開き直した（{nth} 回目のつながり）")
    };
    game_net_note(format!(
        "ネットワーク: ゲーム内のメニューの{how}。受信ソケット {local}（role_nonce {nonce}。待ちは{}）{note}",
        if timer {
            "高分解能の待機タイマー"
        } else {
            "イベントだけ（ms に丸められる）"
        }
    ));
    Ok(Box::new(NetSlot { np, net }))
}

/// ネットワークを開く仕事の本体（開くスレッド）。失敗・パニックは理由を本ログの文（`game_net_note`）へ書き、ゲームのスレッドへは「失敗した」だけを渡す
/// （ゲームのスレッドで `Mutex` を待ったり文字列を作ったりしないため）。
fn open_job(open: impl FnOnce() -> Result<Box<NetSlot>, String>) -> Result<Pending, ()> {
    OPEN_TRIES.fetch_add(1, Ordering::Relaxed);
    let r = catch_unwind(AssertUnwindSafe(open))
        .unwrap_or_else(|_| Err("ネットワークを開く仕事でパニックした".to_string()));
    match r {
        Ok(slot) => Ok(Pending::Play(slot)),
        Err(why) => {
            game_net_problem(format!(
                "ネットワーク: 開けなかった（{why}）。メニューからやり直せる"
            ));
            Err(())
        }
    }
}

/// 観戦者のネットワークを開く仕事の本体（開くスレッド。`open_job` の観戦者バージョン）。失敗・パニックは理由を本ログの文へ書き、ゲームのスレッドへは
/// 「失敗した」だけを渡す。
fn open_watch_job(open: impl FnOnce() -> Result<Box<WatchNetSlot>, String>) -> Result<Pending, ()> {
    OPEN_TRIES.fetch_add(1, Ordering::Relaxed);
    let r = catch_unwind(AssertUnwindSafe(open))
        .unwrap_or_else(|_| Err("観戦者のネットワークを開く仕事でパニックした".to_string()));
    match r {
        Ok(slot) => Ok(Pending::Watch(slot)),
        Err(why) => {
            game_net_problem(format!(
                "観戦: 観戦者のネットワークを開けなかった（{why}）。メニューからやり直せる"
            ));
            Err(())
        }
    }
}

/// 観戦用のルーム名と、左上に出す名（ルーム名の代わり）。一覧から選んだ行があればその行（観戦用のルーム名はハッシュから戻し、
/// 左上は「1P名 vs 2P名」）、無ければ観戦の「ルームマッチ」で打った名。
fn watch_target(link: &UiLink) -> Result<(higanr_core::room::RoomName, String), String> {
    if let Some(row) = link.spectate_pick() {
        let room = row
            .room_name()
            .ok_or("一覧から選んだ行の観戦用のルーム名が決まりの外")?;
        return Ok((room, format!("{} vs {}", row.p1.as_str(), row.p2.as_str())));
    }
    let buf = link
        .spectate_name()
        .ok_or("ルーム名が置かれていない（メニューは名を置いてから指示を送る）")?;
    // メニューも同じガード（`RoomName::parse`）で見て渡すが、ここでも見直す（`RoomNameError` の Debug は位置と数だけで、名の文字を含まない）。
    let name = buf
        .parse()
        .map_err(|e| format!("ルーム名が決まりの外: {e:?}"))?;
    Ok((name.spectate_of(), name.as_str().to_string()))
}

/// 観戦者のネットワークを開く（開くスレッド）: 受信ソケット・STUN・受信スレッド・観戦者のセッション・観戦者のルームのスレッド。接続コードの見張りは作らない
/// （観戦用のコードはルームが運ぶ）。決めたルーム名は `UiLink::spectate_name` から読み、**本ログには書かない**。
fn open_watch_net(lazy: &'static LazyNet) -> Result<Box<WatchNetSlot>, String> {
    let named = lazy.settings_now();
    let s = &named;
    let (watch_room, shown) = watch_target(&lazy.link)?;
    let base = s
        .room_server
        .clone()
        .ok_or("ルームサーバー（room_server）が無い（観戦用のルームへ入れない）")?;
    let glyphs = higanr_core::overlay::ui_glyphs();
    let mut publisher = Publisher::new(
        Arc::clone(&lazy.link),
        sanitize_room(&shown, |c| glyphs.binary_search(&c).is_ok()),
        net_udp::now_unix() as u32,
    );
    publisher.publish(CodeState::SpectateJoining, Event::RoomJoining);
    // 役割の乱数は JOIN の nonce に使う（観戦者の識別。ini に書いてあればその値）。
    let nonce = s.role_nonce.unwrap_or_else(default_nonce);
    let (bind, bind_note) = net_udp::resolve_bind(s)?;
    let (mut net, recv) = net_udp::open_for_code_scoped(s.scope, bind)?;
    let local_addr = net.local_addr();
    let local = local_addr.ok_or("受信ソケットが分からない")?;
    let timer = net.high_resolution_wait();
    // 観戦者の候補（返しに載せる）: 自分の受信ソケットと、ini の `stun` があれば見えたアドレス。IPv4 だけ（受信スレッドの観戦は IPv4 の受信ソケットだけ
    // —— `net_udp::recv_loop_v6`）。
    let mut note = String::new();
    let (mapped, stun) = net_udp::stun_state(s, &mut net, &mut note);
    let own = s
        .candidates_for(local, mapped, None)
        .map_err(|e| format!("{e}{note}"))?;
    net.note_own_candidates(own.as_slice());
    let slot = Arc::new(WatchSlot::new());
    let wp = WatchPlay::new(
        Arc::clone(&slot),
        WatchConfig {
            wait_budget_us: s.wait_budget_us,
            margin_frames: WATCH_MARGIN_FRAMES,
            // 本物のゲームは選択の画面を持つ。
            require_net_selection: true,
        },
    );
    let kit = JoinKit {
        room: RoomOptions::product(base),
        watch_room,
        reader: Reader::new(s, nonce),
        reply_name: higanr_core::handshake::Name::new(&s.name),
        own,
        slot,
        publisher,
    };
    // 起こせなければ `net`・`recv` がここで落ちて受信ソケットが閉じる。
    spectate_join::spawn(kit)?;
    *OPEN_INFO.lock().unwrap_or_else(|e| e.into_inner()) = Some(OpenInfo {
        local: local_addr,
        scope: s.scope,
        stun,
    });
    *RECV.lock().unwrap_or_else(|e| e.into_inner()) = Some(recv);
    start_relink_thread();
    // 観戦者の早送り: 観戦するマッチの方式は繋がるまで分からず、早送りは方式に依らないので、観戦者のネットワークを開くときに走査の call を置く。
    // 書けなくても観戦は続く（早送りが効かないだけ）。
    game_net_note(format!("観戦: 早送り: {}", arm_watch_scan_at_open()));
    let nth = WATCHES_CLOSED.load(Ordering::SeqCst) + 1;
    game_net_note(format!(
        "観戦: 「ルームで観戦」で観戦者のネットワークを開いた（{nth} 回目。ルーム名は記録しない）。受信ソケット {local}（待ちは{}）{bind_note}{note}",
        if timer {
            "高分解能の待機タイマー"
        } else {
            "イベントだけ（ms に丸められる）"
        }
    ));
    Ok(Box::new(WatchNetSlot {
        wp,
        net,
        shots: Default::default(),
    }))
}

/// 開き終わったネットワークのセッションがあれば取って置く（ゲームのスレッド。ネットワークを開いている最中だけ呼ぶ）。取れる錠でなければ次の tick（待たない）。
/// 置けなかった・開けなかったら、メニューへ知らせて `OPEN_IDLE` に戻す。ゲームのスレッドの規律: `try_lock`、文字列を作らない、確保しない
/// （例外は失敗したネットワークのセッションの解放）。失敗の文は開くスレッド（`open_job`）が書く。
fn poll_pending() {
    if OPEN_STATE.load(Ordering::Acquire) != OPEN_BUSY {
        return;
    }
    let taken = match PENDING.try_lock() {
        Ok(mut g) => g.take(),
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner().take(),
        Err(std::sync::TryLockError::WouldBlock) => None,
    };
    let Some(r) = taken else {
        return;
    };
    let placed = match r {
        Ok(Pending::Play(slot)) => install_slot(slot),
        Ok(Pending::Watch(w)) => install_placed(Slot::Watch(w)),
        Err(()) => false,
    };
    if placed {
        return;
    }
    if let Some(l) = LAZY.get() {
        l.link.publish(&Note {
            event: Event::Internal,
            seq: 1,
            ..Note::default()
        });
    }
    OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
}

/// ネットワークのセッションを `NET` へ置いて動かす（ゲームのスレッド。`NET_ON` が 0 のとき）。置けたら true。原子変数だけで決まる確かめ（`not_ready`）を
/// 先にする。直に読むアドレスの確かめは `arm_game_ui` が起動のときに済ませてある。
fn install_slot(slot: Box<NetSlot>) -> bool {
    install_placed(Slot::Play(slot))
}

/// `install_slot` の中身（対戦者のセッションも観戦者のセッションも同じ手順）。観戦者のセッションは、読み取りのスレッドが `_watch_frames.csv` を開く印
/// （`spectate_watch::set_armed`）を先に立て、先頭のフックは置かない。
fn install_placed(slot: Slot) -> bool {
    if NET_ON.load(Ordering::SeqCst) != 0
        || not_ready(SET_STATE_FN.load(Ordering::SeqCst)).is_some()
    {
        INSTALL_FAILS.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    // SAFETY: 引数の無い呼び出し。
    GAME_TID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    // ネットワークの数を 0 から（つなぎ直しの 2 本目から意味がある）。`publish` は CHECK のズレの欄を上書きしないので、消さないと次のつながりの行に
    // 残る。マッチをやめた数もセッションごとに 0 から。確保も待ちも無い（ネットワークのセッションを置く tick に 1 回。マッチの外）。
    NET_STATUS.reset();
    for a in [&RB_ABORTS, &RB_ABORT_FIRST, &RB_ABORT_FRAME] {
        a.store(0, Ordering::Relaxed);
    }
    let play = matches!(slot, Slot::Play(_));
    if !play {
        // 検査では立てない（静的変数が残り、ほかの検査の `_net_frames.csv` の隣に `_watch_frames.csv` が現れる）。
        #[cfg(not(test))]
        crate::spectate_watch::set_armed();
    }
    // SAFETY: `NET_ON` が 0 なので、ネットワークのセッションに触るもう一方（`route`・`match_start_on`・`rb_slot`）は `NET_ON` を見て触らない。ここはゲームの
    // スレッドで、触るのもゲームのスレッドだけ。
    unsafe { *NET.0.get() = Some(slot) };
    NET_ARMED.store(1, Ordering::SeqCst);
    if play {
        install_head_wait();
    }
    NET_ON.store(1, Ordering::Release);
    OPEN_STATE.store(OPEN_DONE, Ordering::SeqCst);
    true
}

/// メニューの入れ物（ゲームのスレッドだけが触る。メニューを置いていない実行は `None`）。tick の中で借りて、その tick の中で返す。
fn game_ui() -> Option<&'static mut NetUi> {
    if UI_ON.load(Ordering::Acquire) == 0 {
        return None;
    }
    // SAFETY: `UI_ON` を Acquire で 1 と読んだので、`arm_game_ui` が置いた入れ物が見える。触るのはゲームのスレッドだけで、借りた参照は
    // 呼び出した tick の中で使い終える。
    unsafe { (*UI.0.get()).as_mut() }
}

/// メニューの数え（`UiStats`）を、読み取りのスレッドが本ログへ読む原子変数へコピーする。
fn publish_ui_stats(ui: &NetUi) {
    let st = ui.stats();
    for (a, v) in UI_STATS.iter().zip([
        st.held_ticks,
        st.opened,
        st.intercepts,
        st.orders_sent,
        st.auto_presses,
        st.confirms,
        st.leaves,
    ]) {
        a.store(v, Ordering::Relaxed);
    }
}

/// ネットワークをまだ開いていない実行（`np` なし）のメニューを 1 tick 進める（ゲームのスレッド）。`UiOut::action` が `Some` なら、メニューが入力を
/// 握って feed を済ませた。最初の「作る」「貼る」ならここでネットワークを開く仕事を頼む（`request_open`）。
fn ui_only_step(game: &mut NetGame<'_>) -> UiOut {
    let Some(ui) = game_ui() else {
        return UiOut::default();
    };
    // 片づけている最中（`OPEN_CLOSING`）は「閉じている」と見せる（人が「作る」を選べば開く頼みになり、つなぎ直しのスレッドが片づけ終えてから開く）。
    let net = match OPEN_STATE.load(Ordering::Relaxed) {
        OPEN_BUSY | OPEN_REOPEN => NetPhase::Opening,
        _ => NetPhase::Closed,
    };
    let f = UiFacts {
        conn: Conn::Waiting,
        seat: Seat::P1,
        now: net_udp::now_unix(),
        net,
        // ネットワークのセッションがまだ無いので、接続の手順の作り直しは無い。
        stall: None,
    };
    let out = ui.tick(game, &f);
    if out.action.is_some() || out.open_net {
        publish_ui_stats(ui);
    }
    if out.open_net {
        request_open();
    }
    if spectate_asked(&out) {
        request_spectate();
    }
    out
}

/// マッチ中の行を更新する（ゲームのスレッド。`h` が `None` なら消す）。材料（`NetPlay::hud_facts`。進め直しの数の差を取るので呼ぶと状態が動く）は
/// **メニューを置いている実行（`UI_ON`）の中だけ**で作る: offer / answer の実行に 0 の上乗せをしない。
fn ui_hud_tick(np: &mut NetPlay) {
    let Some(ui) = game_ui() else {
        return;
    };
    match np.hud_facts() {
        Some(h) => ui.hud(true, h),
        None => ui.hud(false, HudFacts::default()),
    }
}

/// ネットワークをまだ開いていない実行の 1 tick。画面 6 以外・メニューが握らない tick は台本があれば台本、無ければ 0 = poll_all へ返す。画面 6 は
/// メニューが必ず握る（`NetUi::tick` は画面 6 では常に action を返す）ので、画面 6・カーソル 4 の決定がゲームへ漏れる経路は無い。
/// ネットワークのセッションを置いた tick もここへ来ない（`route` が先に置いてネットワークの経路へ回す）。
fn ui_only_tick(mut engine: Option<&mut Engine>, io: &mut RealTickIo, k: u32) -> u32 {
    if NET_ON.load(Ordering::Acquire) != 0 {
        // 来ない見込み（`route` が `NET_ON` を見てから呼ぶ）。来たら、ゲーム自身の poll へ任せず（画面 6・カーソル 4 の決定が漏れうる）、
        // 0 を 3 本流して poll_all を呼ばずに戻る。
        for i in 0..3 {
            io.feed(i, 0);
        }
        return 1;
    }
    let (out, step) = {
        let mut game = NetGame::new(io, engine.as_deref_mut(), load_addrs());
        let out = ui_only_step(&mut game);
        (out, game.script_step)
    };
    if let Some(s) = step {
        // メニューが台本を 1 tick 進めた（自分の入力ワードに使った）。撮影の頼みと進み具合は `net_tick` と同じに出す。
        if s.shot != 0 {
            shot_win::place_request(s.shot, s.step, k);
        }
        publish(s.status);
    }
    match out.action.and_then(net_game::entry_return) {
        Some(ret) => ret,
        None => match engine {
            Some(e) => script_tick(e, io, k, true),
            None => 0,
        },
    }
}

/// 台本の入れ物とアドレスを置く（読み取りのスレッド。`SCRIPT_ON` が 0 の間だけ）。直に読むアドレスが読めない・関数のアドレスが
/// 0 なら断る（置いたものは使われない）。
pub fn arm(script: Script, addrs: GameAddrs, feed_fn: u32, local_fn: u32) -> Result<(), String> {
    arm_io(addrs, feed_fn, local_fn)?;
    // SAFETY: `SCRIPT_ON` が 0 なので、ゲームのスレッドは入れ物に触らない（`EngineCell` の Sync の注。`arm_io` が見た）。
    unsafe { *ENGINE.0.get() = Some(Engine::new(script)) };
    STATE.store(RunState::Running.code(), Ordering::SeqCst);
    Ok(())
}

/// アドレスと関数のアドレスだけを置き、台本の入れ物は空にする（台本の無いネットワークの実行。`SCRIPT_ON` が 0 の間だけ）。断る条件は `arm` と同じ。
/// 入力の入口は、この後にネットワークのセッションを置いて動かしたときだけ開ける（`start`）。
pub fn arm_io(addrs: GameAddrs, feed_fn: u32, local_fn: u32) -> Result<(), String> {
    if SCRIPT_ON.load(Ordering::SeqCst) != 0 {
        return Err("台本がもう動いている".to_string());
    }
    if feed_fn == 0 || local_fn == 0 {
        return Err(format!(
            "呼ぶ関数のアドレスが 0（feed 0x{feed_fn:08X} / ローカル入力 0x{local_fn:08X}）"
        ));
    }
    for (a, len) in addrs.direct_ranges() {
        if a == 0 || read_own_memory(a, len as usize).is_none() {
            return Err(format!(
                "直に読むアドレス 0x{a:08X}（{len} バイト）が読めない"
            ));
        }
    }
    let [s, p, l, g, m, i0, i1, i2, e, u] = &ADDRS;
    s.store(addrs.supervisor, Ordering::SeqCst);
    p.store(addrs.pause, Ordering::SeqCst);
    l.store(addrs.loading, Ordering::SeqCst);
    g.store(addrs.global_state, Ordering::SeqCst);
    m.store(addrs.main_menu_ptr, Ordering::SeqCst);
    i0.store(addrs.input_managers[0], Ordering::SeqCst);
    i1.store(addrs.input_managers[1], Ordering::SeqCst);
    i2.store(addrs.input_managers[2], Ordering::SeqCst);
    e.store(addrs.match_end_menu, Ordering::SeqCst);
    u.store(addrs.gui_ptr, Ordering::SeqCst);
    FEED_FN.store(feed_fn, Ordering::SeqCst);
    LOCAL_FN.store(local_fn, Ordering::SeqCst);
    // 前の実行の poll のアドレスを持ち越さない（置くのは、この後に照合が合ったときの `arm_poll` だけ）。
    POLL_FN.store(0, Ordering::SeqCst);
    for (a, v) in STATE_SLOTS.iter().zip(PLAYER_SLOTS) {
        a.store(v, Ordering::SeqCst);
    }
    // `SCRIPT_ON` が 0 なので、ゲームのスレッドは記録に書いていない。
    STATE_LOG.reset();
    // SAFETY: `SCRIPT_ON` が 0 なので、ゲームのスレッドは入れ物に触らない（`EngineCell` の Sync の注）。
    unsafe { *ENGINE.0.get() = None };
    for a in [&STEP, &LINE, &TICKS, &FED] {
        a.store(0, Ordering::SeqCst);
    }
    STATE.store(RunState::Idle.code(), Ordering::SeqCst);
    Ok(())
}

/// 入口の分岐を開く（`SCRIPT_ON` = 1）。関数のアドレスが置かれていなければ開かない。台本の入れ物が空なら、ネットワークが動いているときだけ開く
/// （台本もネットワークも無いのに開くと、入口は毎 tick Rust を呼ぶだけになる）。
pub fn start() -> Result<(), String> {
    if FEED_FN.load(Ordering::SeqCst) == 0 || LOCAL_FN.load(Ordering::SeqCst) == 0 {
        return Err("関数のアドレスが置かれていない（arm / arm_io の前）".to_string());
    }
    // SAFETY: 読むだけ。`SCRIPT_ON` を 1 にする前なので、ゲームのスレッドは入れ物に触らない。
    if unsafe { (*ENGINE.0.get()).is_none() }
        && NET_ON.load(Ordering::SeqCst) == 0
        && UI_ON.load(Ordering::SeqCst) == 0
    {
        return Err(
            "台本の入れ物が空で、ネットワークもメニューも動いていない（arm か start_net の前）"
                .to_string(),
        );
    }
    SCRIPT_ON.store(1, Ordering::Release);
    Ok(())
}

/// 分岐を閉じる（検査の後始末）。
#[cfg(test)]
pub fn stop() {
    SCRIPT_ON.store(0, Ordering::SeqCst);
    NET_ON.store(0, Ordering::SeqCst);
}

/// ネットワークのセッションと、シードを置く関数のアドレスを置く（読み取りのスレッド。`SCRIPT_ON` と `NET_ON` が 0 の間だけ）。入力の関数のアドレス・描画だけの
/// フレームの出口が置かれていない、直に読み書きするアドレスが読めない、関数のアドレスが 0 なら断る。
pub fn arm_net(slot: NetSlot, set_state_fn: u32) -> Result<(), String> {
    arm_net_with(slot, set_state_fn, &net_game::direct_ranges())
}

/// 観戦者のセッションを置く（`arm_net` の観戦者バージョン。断る条件は同じ）。
pub fn arm_watch(slot: WatchNetSlot, set_state_fn: u32) -> Result<(), String> {
    arm_slot(
        Slot::Watch(Box::new(slot)),
        set_state_fn,
        &net_game::direct_ranges(),
    )
}

/// `arm_net` の中身（読めるかを見る範囲を渡す。検査は自分の静的変数か、確保されていないアドレスを渡す）。
fn arm_net_with(slot: NetSlot, set_state_fn: u32, ranges: &[(u32, u32)]) -> Result<(), String> {
    arm_slot(Slot::Play(Box::new(slot)), set_state_fn, ranges)
}

/// ネットワークのセッションを置いてよい準備が整っているか（関数のアドレス・描画だけのフレームの出口・直に読み書きするアドレス）。`arm_slot` と
/// `arm_game_ui`（ネットワークを開く前にメニューを置くとき）と `install_slot`（後からセッションを置くとき）が同じ確かめをする。
fn check_ready(set_state_fn: u32, ranges: &[(u32, u32)]) -> Result<(), String> {
    match not_ready(set_state_fn) {
        None => {}
        Some(NotReady::Fn) => {
            return Err("関数のアドレスが 0（set_state か、arm / arm_io の前）".to_string());
        }
        Some(NotReady::Exit) => {
            return Err("描画だけのフレームの出口が入口に置かれていない".to_string());
        }
    }
    for &(a, len) in ranges {
        if read_own_memory(a, len as usize).is_none() {
            return Err(format!(
                "直に読み書きするアドレス 0x{a:08X}（{len} バイト）が読めない"
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotReady {
    Fn,
    Exit,
}

/// 関数のアドレスと出口が置かれていなければ、その理由。確保しない・待たない（ゲームのスレッドの `install_slot` が使う。直に読むアドレスの確かめは
/// 起動のときの `arm_game_ui` と、ネットワークを開くスレッドの `open_game_net` が済ませる）。
fn not_ready(set_state_fn: u32) -> Option<NotReady> {
    if set_state_fn == 0
        || FEED_FN.load(Ordering::SeqCst) == 0
        || LOCAL_FN.load(Ordering::SeqCst) == 0
    {
        return Some(NotReady::Fn);
    }
    if stub::RENDER_ONLY_EXIT.load(Ordering::SeqCst) == 0 {
        return Some(NotReady::Exit);
    }
    None
}

fn arm_slot(slot: Slot, set_state_fn: u32, ranges: &[(u32, u32)]) -> Result<(), String> {
    if SCRIPT_ON.load(Ordering::SeqCst) != 0 || NET_ON.load(Ordering::SeqCst) != 0 {
        return Err("入口の分岐がもう開いている".to_string());
    }
    check_ready(set_state_fn, ranges)?;
    SET_STATE_FN.store(set_state_fn, Ordering::SeqCst);
    GAME_TID.store(0, Ordering::SeqCst);
    // SAFETY: `SCRIPT_ON` と `NET_ON` が 0 なので、ゲームのスレッドはネットワークのセッションに触らない（`NetCell` の Sync の注）。
    unsafe { *NET.0.get() = Some(slot) };
    Ok(())
}

/// 素の poll の入口を置く（読み取りのスレッド。`arm_io` の後、`SCRIPT_ON` を 1 にする前。0 なら「置かない」）。
/// 呼ぶのは `session::run` が `POLL_SITES` の先頭 8 バイトを th09.exe の実バイトと照合して合ったときだけ。
pub fn arm_poll(addr: u32) {
    POLL_FN.store(addr, Ordering::SeqCst);
}

/// 立ち絵の 3 本の関数の入口を置く（読み取りのスレッド。`arm_net` の後、`NET_ON` を 1 にする前。3 本とも 0 でないときだけ
/// 置き、1 本でも 0 なら全部 0 に戻す —— 一部だけ呼ぶ形を作らない）。
pub fn arm_portrait(fns: [u32; 3]) {
    let ok = fns.iter().all(|&a| a != 0);
    for (slot, a) in PORTRAIT_FNS.iter().zip(fns) {
        slot.store(if ok { a } else { 0 }, Ordering::SeqCst);
    }
}

/// 受信スレッド（開いたら閉じない。プロセスの終わりまで残す）。ゲーム内のメニューの実行だけは、つながりが終わったらワーカーのスレッドが取り出して
/// 止め（`relink_tick`）、次のつながりを開くスレッドが新しいものを置く。ゲームのスレッドは触らない。
static RECV: std::sync::Mutex<Option<RecvThread>> = std::sync::Mutex::new(None);
/// ネットワークのセッションを置いた（読み取りのスレッドが `_net_frames.csv` を開くかの印）。
static NET_ARMED: AtomicU32 = AtomicU32::new(0);

/// 役割を決める乱数の既定（ini に `nonce` が無いとき。時刻とプロセス番号から。peer_sim の既定と同じ考え方）。
fn default_nonce() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut z = t ^ (u64::from(std::process::id()) << 32);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z ^ (z >> 31)
}

/// 接続コードの見張り（ini の `code_side` の形だけ）。プロセスの終わりまで持つ（繋がった・ネットワークが終わった・諦めたときに自分で終わる）。
/// ゲーム内のメニューの実行は、つながりを片づけるときに止め（文と数は残す）、次のつながりを開くときに置き換える。
static CODE_WATCH: std::sync::Mutex<Option<net_code::Watch>> = std::sync::Mutex::new(None);
/// 観戦用のコードの見張り（ini の `code_side = spectate` の形だけ。M6-3。`WATCH_CODE` も終わりまで持つ）。
static WATCH_CODE: OnceLock<spectate_watch_code::WatchCode> = OnceLock::new();
/// ネットワークを開いたときに分かったこと（受信ソケットのアドレス・範囲・STUN の結果。試験のまとめ）。ゲーム内のメニューの実行は、つながりを開くたびに
/// 置き換える（最後のつながりの値）。
static OPEN_INFO: std::sync::Mutex<Option<OpenInfo>> = std::sync::Mutex::new(None);

/// ネットワークを開いてセッションを置く（読み取りのスレッド）。まだ動かさない（`start_net`）。開いた受信ソケットの文を返す。`code_dir` は接続コードの置き場
/// （`RealScriptHost::arm_net` がゲームのフォルダの外かを見てから渡す）。
pub fn open_net(
    s: &NetSettings,
    set_state_fn: u32,
    code_dir: Option<PathBuf>,
    spectate_dir: Option<PathBuf>,
) -> Result<String, String> {
    if RECV.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return Err("ネットワークはもう開いている".to_string());
    }
    let nonce = s.role_nonce.unwrap_or_else(default_nonce);
    // 接続コードの形で、見張りを起こす前に断ったときは、ウィンドウの「状態」の行に出す文を置き場へ書く（`net_code::say_not_started`）。
    let status_dir = code_dir.clone();
    let not_started = |e: String| {
        if let (Some(side), Some(d)) = (s.code_side, status_dir.as_deref()) {
            net_code::say_not_started(d, side);
        }
        e
    };
    // `bind = auto:ポート` は、ここ（読み取りのスレッド）で経路表を引いてアドレスにする。決められなければネットワークを開かない。
    let (bind, bind_note) = net_udp::resolve_bind(s).map_err(not_started)?;
    let bind_note = if bind_note.is_empty() {
        bind_note
    } else {
        format!(" / {bind_note}")
    };
    let (mut net, th) = match s.code_side {
        None => net_udp::open_scoped(s.scope, bind, &s.peer_addrs())?,
        Some(_) => net_udp::open_for_code_scoped(s.scope, bind).map_err(not_started)?,
    };
    let local_addr = net.local_addr();
    let local = local_addr.map_or("（分からない）".to_string(), |a| a.to_string());
    let timer = net.high_resolution_wait();
    if s.code_side == Some(CodeSide::Spectate) {
        // 観戦者: 対戦のセッションは作らず、観戦用のコードの見張りと `WatchPlay` を置く。
        let Some(dir) = code_dir else {
            return Err(not_started(
                "接続コードの置き場が決まっていない".to_string(),
            ));
        };
        let slot = std::sync::Arc::new(WatchSlot::new());
        let (w, note) = spectate_watch_code::start(
            s,
            dir,
            nonce,
            std::sync::Arc::clone(&slot),
            net_code::Timing::default(),
        )
        .map_err(not_started)?;
        let wp = WatchPlay::new(
            slot,
            WatchConfig {
                wait_budget_us: s.wait_budget_us,
                margin_frames: WATCH_MARGIN_FRAMES,
                // 本物のゲームは選択の画面を持つ。
                require_net_selection: true,
            },
        );
        // 断られたら `th` と見張り（`w`）はここで落ちて止まる。
        arm_watch(
            WatchNetSlot {
                wp,
                net,
                shots: Default::default(),
            },
            set_state_fn,
        )?;
        // 読み取りのスレッドが `_watch_frames.csv` を開く印（`script_watch`）。検査の `arm_watch` は付けない（静的変数が残るため）。
        crate::spectate_watch::set_armed();
        let _ = WATCH_CODE.set(w);
        *OPEN_INFO.lock().unwrap_or_else(|e| e.into_inner()) = Some(OpenInfo {
            local: local_addr,
            scope: s.scope,
            stun: crate::trial_summary::Stun::NotUsed,
        });
        *RECV.lock().unwrap_or_else(|e| e.into_inner()) = Some(th);
        NET_ARMED.store(1, Ordering::SeqCst);
        return Ok(format!(
            "受信ソケット {local} を開き、観戦者のセッションを置いた（見るだけの側。待ちは{}）{bind_note}{note}",
            if timer {
                "高分解能の待機タイマー"
            } else {
                "イベントだけ（ms に丸められる）"
            }
        ));
    }
    // rollback の入口を全部書けた実行か。HELLO で名乗る形（ini の形も、ウィンドウで選んだ形も）はこれを見る。
    let rb_wired = crate::rollback_play_win::armed();
    let cfg = play_config(s, rb_wired);
    // 秘密（ini か OS の乱数）・STUN・セッションの設定は通信層で作る（`net_udp::prepare_link`）。接続コードの形では、セッションの設定は見張りのスレッドが
    // 相手のコードを読んでから置く。
    let (np, note, watch, stun) = match (s.code_side, code_dir) {
        (None, _) => {
            let (link_cfg, note, stun) = net_udp::prepare_link_stun(s, &mut net, nonce)?;
            (NetPlay::new(link_cfg, cfg, &mut net), note, None, stun)
        }
        (Some(_), Some(dir)) => {
            let slot = std::sync::Arc::new(netplay::LinkSlot::new());
            let (w, note) = net_code::start(
                s,
                dir,
                &mut net,
                nonce,
                std::sync::Arc::clone(&slot),
                net_code::Timing::default(),
                crate::notify_win::connected,
                // 接続コードのウィンドウで選んだ形（`mode.txt`）を、自分のプロセス番号と合うときだけ採る（前の実行の残りを使わない）。
                net_code::ModeCtx::new(s, std::process::id(), rb_wired),
            )
            .map_err(not_started)?;
            // STUN の結果は見張りが持つ（受信ソケットを開くときに問い合わせた）。
            let stun = w.facts().stun;
            (
                NetPlay::with_code_slot(slot, cfg, &mut net),
                note,
                Some(w),
                stun,
            )
        }
        (Some(_), None) => return Err("接続コードの置き場が決まっていない".to_string()),
    };
    // 観戦のホスト（ini の `[spectate] enable = 1`）: リングバッファをセッションに置き、観戦用のコードを用意する。使わない設定なら何もしない。
    let mut np = np;
    let (spectate, spectate_note) = spectate_host::arm(s, spectate_dir, local_addr, &mut np);
    // 断られたら `th` と見張り（`watch`）はここで落ちて止まる。
    arm_net(NetSlot { np, net }, set_state_fn)?;
    let spectate_note = match spectate {
        Some(p) => p.commit(&th),
        None => spectate_note,
    };
    if let Some(w) = watch {
        *CODE_WATCH.lock().unwrap_or_else(|e| e.into_inner()) = Some(w);
    }
    *OPEN_INFO.lock().unwrap_or_else(|e| e.into_inner()) = Some(OpenInfo {
        local: local_addr,
        scope: s.scope,
        stun,
    });
    KEEP_AFTER_SCRIPT.store(
        u32::from(s.after_script == AfterScript::Keep),
        Ordering::SeqCst,
    );
    *RECV.lock().unwrap_or_else(|e| e.into_inner()) = Some(th);
    NET_ARMED.store(1, Ordering::SeqCst);
    Ok(format!(
        "受信ソケット {local} を開き、セッションを置いた（role_nonce {nonce}。待ちは{}）{bind_note}{note}{spectate_note}",
        if timer {
            "高分解能の待機タイマー"
        } else {
            "イベントだけ（ms に丸められる）"
        }
    ))
}

pub fn net_armed() -> bool {
    NET_ARMED.load(Ordering::SeqCst) != 0
}

/// 試験のまとめの材料のうち、静的な値から引くもの（読み取りのスレッド。ネットワークを開いていなければ `open` と `code` は None）。
pub fn trial_host() -> HostFacts {
    HostFacts {
        open: OPEN_INFO.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        code: CODE_WATCH
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(net_code::Watch::facts)
            .or_else(|| WATCH_CODE.get().map(spectate_watch_code::WatchCode::facts)),
        net_panicked: NET_PANICKED.load(Ordering::Relaxed) != 0,
        replay: crate::replay_save_win::facts(),
        menu: menu_net(),
    }
}

/// ゲーム内のメニューの実行の、ネットワークの開き方の段階（読み取りのスレッド）。頼みが来たかは、開くスレッドの数（`OPEN_TRIES`）か、スレッドを
/// 起こせなかった文（`game_net_note`）で見る。
fn menu_net() -> MenuNet {
    let noted = GAME_NET_NOTES
        .lock()
        .map(|g| !g.is_empty())
        .unwrap_or(false);
    menu_net_of(
        LAZY.get().is_some(),
        OPEN_TRIES.load(Ordering::Relaxed) > 0 || noted,
    )
}

fn menu_net_of(menu_placed: bool, asked: bool) -> MenuNet {
    match (menu_placed, asked) {
        (false, _) => MenuNet::NotMenu,
        (true, false) => MenuNet::NotAsked,
        (true, true) => MenuNet::Asked,
    }
}

/// ネットワークの数と、ネットワークの外の数の文（読み取りのスレッド。ログ用）。
pub fn net_status() -> (NetSnapshot, String) {
    let (recv, spec, diag) = {
        let g = RECV.lock().unwrap_or_else(|e| e.into_inner());
        (
            g.as_ref().map_or("受信スレッドなし".to_string(), |t| {
                format!("受信 {:?}", t.stats())
            }),
            // 観戦のホストの集計（`[spectate] enable = 1` のときだけ 1 行。無ければ今までの行のまま）。
            g.as_ref()
                .and_then(RecvThread::spectate_snapshot)
                .map(|s| format!("\n{}", s.text()))
                .unwrap_or_default(),
            // 受信側が持つ診断（STUN の聞き直し・送りの誤り・自分の候補。受信スレッドが無ければ全部 0 / なし）。
            g.as_ref().map(RecvThread::diag).unwrap_or_default(),
        )
    };
    // 片づけたつながりの見張りの文は、片づけたときに「n 回目のつながり」の節へコピーしたので、ここでは出さない。
    let code = if CODE_NOTES_ARCHIVED.load(Ordering::SeqCst) {
        String::new()
    } else {
        CODE_WATCH
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|w| {
                w.notes()
                    .iter()
                    .map(|l| format!("\n{l}"))
                    .collect::<String>()
            })
            .unwrap_or_default()
    };
    // ゲーム内のメニューの実行: ネットワークを開く仕事の文と、メニューの数え（`code_side = game` の実行だけ）。
    let code = GAME_NET_NOTES
        .lock()
        .map(|g| g.iter().map(|l| format!("\n{l}")).collect::<String>())
        .unwrap_or_default()
        + &code
        + &ui_stats_text();
    // 観戦者（`code_side = spectate`）の見張りの文と集計（観戦者の実行だけ。無ければ今までの行のまま）。
    let code = code
        + &WATCH_CODE
            .get()
            .map(|w| {
                w.notes()
                    .iter()
                    .map(|l| format!("\n{l}"))
                    .collect::<String>()
            })
            .unwrap_or_default()
        + &if WATCH_STATUS.published() {
            format!("\n{}", WATCH_STATUS.text())
        } else {
            String::new()
        }
        // 観戦者のルーム（「ルームで観戦」。観戦用のルームの出来事の数。起こしたことが無ければ空）。
        + &spectate_requests_text()
        + &spectate_join::stats_text()
        + &PAST_WATCHES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .concat();
    // ゲーム内のメニューの実行は、つながりが終わってもネットワークを開き直せる（`NetSnapshot::ended` は「この起動ではもう開かない」の意味になり、
    // 計測はネットワークの 1 本の終わりで締めない）。パニックでネットワークとメニューを止めた実行は開き直せない。
    let mut snap = NET_STATUS.snapshot();
    snap.relink = UI_ON.load(Ordering::SeqCst) != 0;
    snap.diag = diag;
    // 繋がらなかった理由を読むための診断（セッションを作った後・STUN を使った実行だけ。人を特定する値は書かない）。
    let diag_line = if snap.has_link_box() || diag.keep.installed || diag.port_kept.is_some() {
        format!("\n{}", snap.diag_text())
    } else {
        String::new()
    };
    (
        snap,
        format!(
            "{recv} / 開始のフックを通った {} 回（ゲームのスレッドでないので触らなかった {} 回） / ネットワークの経路のパニック {}\n{}",
            stub::MATCH_CALLS.load(Ordering::Relaxed),
            MATCH_FOREIGN.load(Ordering::Relaxed),
            if NET_PANICKED.load(Ordering::Relaxed) != 0 {
                "あり（ネットワークを止めた）"
            } else {
                "なし"
            },
            NET_STATUS.menu_text(),
        ) + "\n"
            + &NET_STATUS.timing_text()
            + "\n"
            + &STATE_LOG.text()
            + &code
            + &diag_line
            + &spec
            + &rollback_text()
            + &save_skip_text()
            + &past_links_text(),
    )
}

/// 「ルームで観戦」の頼みの数（本ログの 1 行。頼みが 1 度も来ていない実行は空）。
fn spectate_requests_text() -> String {
    let (n, refused) = (
        SPECTATE_REQUESTS.load(Ordering::Relaxed),
        SPECTATE_REFUSED.load(Ordering::Relaxed),
    );
    if n == 0 {
        return String::new();
    }
    format!(
        "\n観戦の頼み: 「ルームで観戦」を決めた {n} 回（セッションが使われていて断った {refused} 回。観戦者のネットワークを開いた回数は「観戦: …観戦者のネットワークを開いた」の行）"
    )
}

/// 片づけたつながりの節（つなぎ直した実行だけ。本ログの「ネットワークの外」の終わりに足す）と、上のネットワークの行が何回目のつながりかの 1 行。
fn past_links_text() -> String {
    let past = PAST_LINKS.lock().unwrap_or_else(|e| e.into_inner());
    if past.is_empty() {
        return String::new();
    }
    let closed = LINKS_CLOSED.load(Ordering::SeqCst);
    let now = if CODE_NOTES_ARCHIVED.load(Ordering::SeqCst) {
        format!(
            "（上の「ネットワーク:」の行とネットワークの数は {closed} 回目のつながりの最後の値。次のつながりはまだ開いていない）"
        )
    } else {
        format!(
            "（上の「ネットワーク:」の行とネットワークの数は {} 回目のつながりのぶん。前のつながりは下の節）",
            closed + 1
        )
    };
    format!("\n{now}{}", past.concat())
}

/// rollback のモードの数え（本ログの「ネットワークの外」に足す。入口を書けていない・一度もコピーしていない実行は空）。
fn rollback_text() -> String {
    rollback_text_with(
        crate::rollback_play_win::summary_text(),
        RbCounts {
            nested: MATCH_NESTED.load(Ordering::Relaxed),
            stray: RB_STRAY.load(Ordering::Relaxed),
            aborts: RB_ABORTS.load(Ordering::Relaxed),
            first: RB_ABORT_FIRST.load(Ordering::Relaxed),
            first_frame: RB_ABORT_FRAME.load(Ordering::Relaxed),
            bypass: RB_BYPASS.load(Ordering::Relaxed),
        },
    )
}

#[derive(Debug, Clone, Copy, Default)]
struct RbCounts {
    nested: u32,
    stray: u32,
    /// マッチをやめた回数・最初の経路（`abort_code`。0 はやめていない）・そのときまでに確定したフレームの数。
    aborts: u32,
    first: u32,
    first_frame: u32,
    /// rollback のマッチの間に、フックが入れ物を借りられず走査を普段どおりに回した回数。
    bypass: u32,
}

/// マッチをやめた経路の番号（`RB_ABORT_FIRST`。0 はやめていない）。
fn abort_code(a: netplay::RbAbort) -> u32 {
    match a {
        netplay::RbAbort::CoreFault => 1,
        netplay::RbAbort::ScanBroken => 2,
        netplay::RbAbort::PostBroken => 3,
        netplay::RbAbort::Stray => 4,
    }
}

fn abort_of_code(c: u32) -> Option<netplay::RbAbort> {
    [
        netplay::RbAbort::CoreFault,
        netplay::RbAbort::ScanBroken,
        netplay::RbAbort::PostBroken,
        netplay::RbAbort::Stray,
    ]
    .into_iter()
    .find(|&a| abort_code(a) == c)
}

/// マッチをやめた回数の行の先頭（`rollback_text_with` が書き、読み取りのスレッドが簡潔なログへコピーする）。
pub const ROLLBACK_ABORT_HEAD: &str = "巻き戻し（配線）: 試合をやめた";

fn rollback_text_with(summary: Option<String>, c: RbCounts) -> String {
    let RbCounts {
        nested,
        stray,
        aborts,
        first,
        first_frame,
        bypass,
    } = c;
    let Some(t) = summary else {
        return String::new();
    };
    let mut s = format!("\n{t}");
    if aborts != 0 {
        let why = abort_of_code(first).map_or("?", |a| a.text());
        s += &format!(
            "\n{ROLLBACK_ABORT_HEAD} {aborts} 回。最初は「{why}」（確定したフレーム {first_frame} の後）"
        );
    }
    if bypass != 0 {
        s += &format!(
            "\n巻き戻し（配線）: rollback の試合の間に、フックが入れ物を借りられず走査を普段どおりに回した {bypass} 回"
        );
    }
    if nested != 0 {
        s += &format!(
            "\n巻き戻し（製品の経路）: フックが入れ物を持っている間に来た開始のフック {nested} 回（触らなかった）"
        );
    }
    if stray != 0 {
        s += &format!(
            "\n巻き戻し（製品の経路）: フックが入れ物を持っている間に、指図が無いまま来た入力の入口 {stray} 回（poll_all を呼ばずに戻した）"
        );
    }
    s
}

/// ネットワークを動かす（`NET_ON` = 1）。`arm_net` の後、入力の分岐（`start`）より前に呼ぶ。
pub fn start_net() -> Result<(), String> {
    // SAFETY: 読むだけ。`NET_ON` を 1 にする前なので、ゲームのスレッドはネットワークのセッションに触らない。
    if unsafe { (*NET.0.get()).is_none() } {
        if UI_ON.load(Ordering::SeqCst) != 0 {
            // ゲーム内のメニューだけの実行（ネットワークは最初の作る・貼るで開く）。`NET_ON` はネットワークのセッションを置くときに立てる。
            return Ok(());
        }
        return Err("ネットワークのセッションが空（arm_net の前）".to_string());
    }
    install_head_wait();
    NET_ON.store(1, Ordering::Release);
    Ok(())
}

pub fn progress() -> Progress {
    let l = |a: &AtomicU32| a.load(Ordering::Acquire);
    Progress {
        state: RunState::from_code(l(&STATE)),
        step: l(&STEP),
        line: l(&LINE),
        ticks: l(&TICKS),
        fed: l(&FED),
        on: l(&SCRIPT_ON) != 0,
    }
}

type FeedFn = unsafe extern "fastcall" fn(u32, u32);
type LocalFn = unsafe extern "C" fn() -> u32;
/// 素の poll `0x0042B850`: ecx = 席（0〜2）、スタックの引数なし、素の `ret`、戻り値 ax（上位 16 ビットは不定）。
type PollFn = unsafe extern "fastcall" fn(u32) -> u32;
type SetStateFn = unsafe extern "thiscall" fn(u32, u32);
/// 立ち絵の 3 本（`0x00424EDD` / `0x00424FB0` / `0x00425233`）: thiscall で ecx = MainMenu、スタックの引数 4 つ、`ret 0x10`。
type PortraitFnPtr = unsafe extern "thiscall" fn(u32, u32, u32, u32, u32);

struct RealTickIo {
    feed: FeedFn,
    local: LocalFn,
    /// 素の poll（`POLL_FN`。照合が合わなければ `None` で、ローカル入力は `local` で読む）。
    poll: Option<PollFn>,
    /// InputManager[0]〜[2] のアドレス（`GameAddrs::input_managers`）。poll が書く範囲をコピーして戻す先（席 s の poll は [s] を書く）。
    ims: [u32; 3],
    set_state: u32,
    portrait: [u32; 3],
}

/// 素の poll の席 `seat`（0〜2）を 1 回呼び、下位 16 ビット（ax）を返す。席 0 は 1P の設定どおりに読んだ入力ワード（ネットワークの対戦の自分の入力）、席 1 は
/// 2P の設定、席 2 は `poll_all` の最後の呼び（画面 6 など `[2]` を読む画面の入力ワード）。InputManager[席] の `+0x00`〜`+0x29`（`IM_BYTES` バイト）は、
/// 呼ぶ前にコピー、呼んだ直後に書き戻す: poll は入力ワードを返すだけでなく今の入力ワード `+0x00`・前の入力ワード `+0x02`・縁 `+0x06` / `+0x08`・リピートの数え
/// `+0x0A`〜`+0x29` を書くので、書きっぱなしだと続く `feed(席, w)` が「前の入力ワード」を取り違え、押した瞬間とリピートが両者で食い違う。
///
/// # Safety
/// `poll` は素の poll `0x0042B850` の入口（`POLL_SITES` の照合が合ったもの）か、同じ約束の検査の関数であること。約束: ecx = 席（0〜2。
/// `poll_all` `0x0042E9C0` も 0・1・2 で呼ぶ）、スタックの引数なし、素の `ret`、戻り値 ax、ebx・esi・edi・ebp を保つ（`0x0042B85E`〜
/// `0x0042B860` で push、`0x0042BE29`〜`0x0042BE31` で pop）。書く先は InputManager[席]（`0x4ACE18 + 席 * 0x8E`）の `+0x00`〜`+0x29` と
/// スタックだけ。`im` は同じ席の InputManager の先頭で、`IM_BYTES` バイト読み書きできること。ゲームのスレッドから呼ぶこと。
unsafe fn poll_seat_keeping_im(poll: PollFn, seat: u32, im: *mut u8) -> u32 {
    let mut saved = [0u8; IM_BYTES];
    // SAFETY: `im` は `IM_BYTES` バイト読める（呼び出し側の約束）。先はスタックの配列で重ならない。
    unsafe { core::ptr::copy_nonoverlapping(im, saved.as_mut_ptr(), IM_BYTES) };
    // SAFETY: `poll` の約束は上の Safety の節。
    let ax = unsafe { poll(seat) };
    // SAFETY: コピーの逆向き。`im` は `IM_BYTES` バイト書ける。ゲームのスレッドだけがこの欄を書く（feed と poll_all も同じスレッド）ので、
    // 読み手が途中の値を見ることはない。
    unsafe { core::ptr::copy_nonoverlapping(saved.as_ptr(), im, IM_BYTES) };
    // 上位 16 ビットは不定（`0x0042BE2A mov %si,%ax` は ax だけを書く）。
    ax & 0xFFFF
}

/// 検査だけ: ゲームのメモリの代わりの表（アドレス → 値）。有効にしている間、`RealTickIo` のゲームのメモリの読みはこの表を引く（無いアドレスは 0）。
/// 本物の入口を偽のゲームの画面で通すため。
#[cfg(test)]
pub(crate) mod fake_mem {
    use core::sync::atomic::{AtomicU32, Ordering};

    const N: usize = 48;
    static ON: AtomicU32 = AtomicU32::new(0);
    static KEYS: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];
    static VALS: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];

    pub fn enable() {
        clear();
        ON.store(1, Ordering::SeqCst);
    }

    pub fn on() -> bool {
        ON.load(Ordering::SeqCst) != 0
    }

    pub fn clear() {
        ON.store(0, Ordering::SeqCst);
        for k in KEYS.iter().chain(VALS.iter()) {
            k.store(0, Ordering::SeqCst);
        }
    }

    pub fn set(addr: u32, v: u32) {
        assert_ne!(addr, 0);
        let i = KEYS
            .iter()
            .position(|k| k.load(Ordering::SeqCst) == addr)
            .or_else(|| KEYS.iter().position(|k| k.load(Ordering::SeqCst) == 0))
            .expect("偽のメモリの表が一杯");
        KEYS[i].store(addr, Ordering::SeqCst);
        VALS[i].store(v, Ordering::SeqCst);
    }

    /// 無効なら `None`（本物の読みへ）。有効なら表の値（無ければ 0）。
    pub fn get(addr: u32) -> Option<u32> {
        if ON.load(Ordering::SeqCst) == 0 {
            return None;
        }
        Some(
            KEYS.iter()
                .position(|k| k.load(Ordering::SeqCst) == addr)
                .map_or(0, |i| VALS[i].load(Ordering::SeqCst)),
        )
    }
}

impl TickIo for RealTickIo {
    fn data_u32(&mut self, addr: u32) -> u32 {
        #[cfg(test)]
        if let Some(v) = fake_mem::get(addr) {
            return v;
        }
        // SAFETY: `addr` は `GameAddrs::direct_ranges` のアドレス（`arm` が読めると確かめた。花映塚では th09.exe の .data で、プロセスが終わるまでコミット済みで読める）。
        // `scripted::read_obs` はそれ以外を直には読まない。
        unsafe { (addr as usize as *const u32).read_unaligned() }
    }
    fn safe_u32(&mut self, addr: u32) -> Option<u32> {
        #[cfg(test)]
        if let Some(v) = fake_mem::get(addr) {
            return Some(v);
        }
        OwnProcess.u32_at(addr)
    }
    fn read_im(&mut self, addr: u32, out: &mut [u8; IM_BYTES]) {
        // 検査: ゲームの InputManager の本物のアドレスは、偽の 3 本を読む。
        #[cfg(test)]
        if fake_mem::on()
            && let Some(i) = crate::th09_addr::INPUT_MANAGERS
                .iter()
                .position(|&a| a == addr)
        {
            for (o, a) in out.iter_mut().zip(tests::FAKE_IMS[i].iter()) {
                *o = a.load(Ordering::SeqCst);
            }
            return;
        }
        // SAFETY: `addr` は InputManager の 3 本のアドレス（`arm` が `IM_BYTES` バイト読めると確かめた。花映塚では .data）。
        // 先はスタックの配列で重ならない。
        unsafe {
            core::ptr::copy_nonoverlapping(addr as usize as *const u8, out.as_mut_ptr(), IM_BYTES)
        };
    }
    fn local_input(&mut self) -> u32 {
        // 本人の入力は 1P の設定どおりに読む（席が 2P でも同じ）。素の poll の席 0 を InputManager[0] を汚さずに呼ぶ。
        // poll の入口の照合が合わなかった実行（`POLL_FN` = 0）は今までの読み方へ戻る。
        if let Some(poll) = self.poll
            && self.ims[0] != 0
        {
            // SAFETY: `poll` は `POLL_SITES` の `fn_poll` 行（`session::arm_poll` が実バイトと照合して一致したときだけ置いた入口）。
            // `ims[0]` は `arm_io` が置いた InputManager[0]。ゲームのスレッドから呼ぶ。
            return unsafe { poll_seat_keeping_im(poll, 0, self.ims[0] as usize as *mut u8) };
        }
        // SAFETY: `local` は `fn_local_input` 行（照合で一致した入口）。引数なし・戻り値 ax・ebx と edi に触らず esi と ebp を保ち、
        // 素の `ret` で戻るので、cdecl の引数なしの関数として呼んでよい。ゲームのスレッドから呼ぶ。
        unsafe { (self.local)() }
    }
    fn feed(&mut self, index: u32, word: u32) {
        // SAFETY: `feed` は `fn_feed` 行（照合で一致した入口）。ecx = 番号（0〜2）、edx = 入力ワード、スタックの引数なし、esi・edi を保ち
        // ebx・ebp に触らないので fastcall の 2 引数の関数として呼んでよい（fastcall との対応は推定）。番号は `scripted::tick` が 0・1・2 だけを渡す。
        unsafe { (self.feed)(index, word) }
    }
}

impl NetIo for RealTickIo {
    fn poll_words(&mut self) -> Option<[u32; 3]> {
        let poll = self.poll?;
        if self.ims.contains(&0) {
            return None;
        }
        // SAFETY: `poll` と `ims` は `local_input` と同じ（席 0 の代わりに 0・1・2。`poll_all` `0x0042E9C0` も同じ 3 つの席で呼ぶ）。
        // 3 本とも `IM_BYTES` バイト読めると確かめてある。ゲームのスレッドから呼ぶ。
        Some([0u32, 1, 2].map(|s| unsafe {
            poll_seat_keeping_im(poll, s, self.ims[s as usize] as usize as *mut u8)
        }))
    }
    fn text_gate(&mut self, open: bool) -> bool {
        crate::text_input_win::set_gate(open)
    }
    fn typed_units(&mut self, out: &mut [u16]) -> usize {
        crate::text_input_win::drain(out)
    }
    fn write_data_u32(&mut self, addr: u32, v: u32) {
        // SAFETY: `addr` は `net_game::direct_ranges` のアドレス（`arm_net` が読めると確かめた th09.exe の .data）。`NetGame` はそれ以外へは
        // 直に書かない。ゲームのスレッド（ゲーム自身がその欄を書くのと同じスレッド）から書く。
        unsafe { (addr as usize as *mut u32).write_unaligned(v) }
    }
    fn safe_write_u8(&mut self, addr: u32, v: u8) -> bool {
        let mut n: usize = 0;
        // SAFETY: 自分のプロセスの擬似ハンドルと、書くアドレス・書く元・長さ・書けたバイト数の書く先を渡すだけ。アドレスが確保されていない・
        // 書けないなら、OS が失敗を返す。
        let ok = unsafe {
            WriteProcessMemory(
                GetCurrentProcess(),
                addr as usize as *const core::ffi::c_void,
                &v as *const u8 as *const core::ffi::c_void,
                1,
                &mut n,
            )
        };
        ok != 0 && n == 1
    }
    fn safe_write_u32(&mut self, addr: u32, v: u32) -> bool {
        write_own_u32(addr, v)
    }
    fn set_rng_state(&mut self, seed: u16) {
        if self.set_state == 0 {
            return;
        }
        // SAFETY: `set_state` は `fn_set_state` 行（照合で一致した入口）。thiscall で ecx = 乱数のアドレス `RNG_ADDR`、スタックの引数 1 つ（u16 を 32 ビットに広げて積む）、
        // `ret $0x4` で自分で片付ける（ゲーム自身の呼び出し `0x00420A41`〜`0x00420A47` も `push %edx`）。関数のアドレスと整数は 32 ビットで同じ大きさ。
        // ゲームのスレッドから呼ぶ（ゲーム自身も起動のスレッドで呼ぶ）。
        unsafe {
            let f = core::mem::transmute::<usize, SetStateFn>(self.set_state as usize);
            f(RNG_ADDR, u32::from(seed))
        }
    }
    fn read_data(&mut self, addr: u32, out: &mut [u8]) {
        #[cfg(test)]
        if fake_mem::on() {
            for (i, chunk) in out.chunks_mut(4).enumerate() {
                let v = fake_mem::get(addr.wrapping_add(4 * i as u32)).unwrap_or(0);
                let n = chunk.len();
                chunk.copy_from_slice(&v.to_le_bytes()[..n]);
            }
            return;
        }
        // SAFETY: `[addr, addr + out.len())` は `net_game::direct_ranges` の範囲の中（`arm_net` が読めると確かめた .data）。
        // 先はスタックの配列で重ならない。
        unsafe {
            core::ptr::copy_nonoverlapping(addr as usize as *const u8, out.as_mut_ptr(), out.len())
        };
    }
    fn portrait_armed(&mut self) -> bool {
        self.portrait.iter().all(|&a| a != 0)
    }
    fn call_portrait(&mut self, m: u32, call: PortraitCall) {
        let a = self.portrait[call.f.index()];
        if a == 0 || m == 0 {
            return;
        }
        let [a0, a1, a2, a3] = call.args;
        // SAFETY: `a` は `PORTRAIT_FNS` の 1 本（`session::arm_portrait` が実バイトと照合して合ったときだけ置いた）。thiscall で
        // ecx = MainMenu `[0x4AC8C0]`（`PortraitGate::ready` が 0 でないことと、`[m+0x11B98]`（部品の配列）が 0 でないことを確かめた直後）、スタックの引数 4 つ、`ret $0x10` で自分で片付ける
        // （`0x00424FAD` / `0x0042505C` / `0x0042538D`）。ebx・esi・edi・ebp は保存して戻る。ゲームのスレッド（ゲーム自身が MainMenu のハンドラでこの 3 本を呼ぶのと同じスレッド）から呼ぶ。
        // 引数は `net_portrait::plan` が作った 4 つで、カーソル（0〜15 に絞り済み）から表で引いたキャラと、ゲーム自身が渡すのと同じ定数。
        // 関数のアドレスと整数は 32 ビットで同じ大きさ。
        unsafe {
            let f = core::mem::transmute::<usize, PortraitFnPtr>(a as usize);
            f(m, a0, a1, a2, a3)
        }
    }
}

impl StateMem for RealTickIo {
    fn loading(&mut self) -> bool {
        // 読み込みのアドレスは `GameAddrs::loading`（`arm_io` が 4 バイト読めると確かめた .data）。
        let a = ADDRS[2].load(Ordering::Relaxed);
        TickIo::data_u32(self, a) != 0
    }
    fn read_u32(&mut self, addr: u32) -> Option<u32> {
        OwnProcess.u32_at(addr)
    }
    fn write_u32(&mut self, addr: u32, v: u32) -> bool {
        // Player はヒープなので、直に書かず落ちない書き方で（アドレスが確保されていなければ失敗で返る）。
        NetIo::safe_write_u32(self, addr, v)
    }
}

fn load_addrs() -> GameAddrs {
    let l = |i: usize| ADDRS[i].load(Ordering::Relaxed);
    GameAddrs {
        supervisor: l(0),
        pause: l(1),
        loading: l(2),
        global_state: l(3),
        main_menu_ptr: l(4),
        input_managers: [l(5), l(6), l(7)],
        match_end_menu: l(8),
        gui_ptr: l(9),
    }
}

fn publish(s: Status) {
    let (step, line) = match s {
        Status::Running { step } => (step as u32, 0),
        Status::Done => (STEP.load(Ordering::Relaxed), 0),
        Status::Aborted { step, line } => (step as u32, line),
    };
    STEP.store(step, Ordering::Relaxed);
    LINE.store(line, Ordering::Relaxed);
    STATE.store(RunState::of(s).code(), Ordering::Release);
}

/// 入力の入口（`stub.rs`）が、`SCRIPT_ON` が 0 でないときに tick の番号 `k` を渡して呼ぶ。0 = 元の飛び先へ、1 = poll_all を呼ばずに戻る、
/// 2 = 描画だけのフレーム（ネットワークの経路だけが返す）。パニックを受けたら台本・ネットワーク・撮影を止めて 0 を返す（ゲームは元の入力で続く）。
pub extern "C" fn script_on_tick(k: u32) -> u32 {
    match catch_unwind(AssertUnwindSafe(|| tick_inner(k))) {
        Ok(r) => r,
        Err(_) => {
            SCRIPT_ON.store(0, Ordering::SeqCst);
            UI_ON.store(0, Ordering::SeqCst);
            if NET_ON.swap(0, Ordering::SeqCst) != 0 {
                NET_PANICKED.store(1, Ordering::SeqCst);
            }
            // rollback のモードのフックが開けていた確保の向け先と見張りを閉じる（ネットワークが止まった後はフックが動かないので、開いたまま残さない）。
            crate::rollback_win::RB_REDIRECT.store(0, Ordering::SeqCst);
            crate::rollback_win::RB_WATCH.store(0, Ordering::SeqCst);
            shot_win::set_off();
            // 保存の画面を飛ばす印を立てたままなら落とす（入口が閉じると見張れない。残すと継続の開始から画面 15 へ来たときに飛ぶ）。
            save_skip_abandon();
            STATE.store(RunState::Panicked.code(), Ordering::SeqCst);
            0
        }
    }
}

fn real_io() -> Option<RealTickIo> {
    let (f, l) = (
        FEED_FN.load(Ordering::Acquire),
        LOCAL_FN.load(Ordering::Acquire),
    );
    if f == 0 || l == 0 {
        return None;
    }
    // SAFETY: `f` と `l` は `arm` が置いた関数の入口のアドレス（0 でないことを上で見た）。`p` は `arm_poll` が照合の後に置いた poll の入口で、
    // 0 なら `None`。関数のアドレスと整数は 32 ビットで同じ大きさ。呼ぶときの約束は `RealTickIo` の各関数の注。
    Some(unsafe {
        let p = POLL_FN.load(Ordering::Acquire);
        RealTickIo {
            feed: core::mem::transmute::<usize, FeedFn>(f as usize),
            local: core::mem::transmute::<usize, LocalFn>(l as usize),
            poll: (p != 0).then(|| core::mem::transmute::<usize, PollFn>(p as usize)),
            ims: [5, 6, 7].map(|i| ADDRS[i].load(Ordering::Relaxed)),
            set_state: SET_STATE_FN.load(Ordering::Acquire),
            portrait: [0, 1, 2].map(|i| PORTRAIT_FNS[i].load(Ordering::Acquire)),
        }
    })
}

fn tick_inner(k: u32) -> u32 {
    if SCRIPT_ON.load(Ordering::Acquire) == 0 {
        return 0;
    }
    let Some(mut io) = real_io() else {
        SCRIPT_ON.store(0, Ordering::SeqCst);
        return 0;
    };
    #[cfg(test)]
    if PANIC_NEXT.swap(false, Ordering::SeqCst) {
        panic!("検査: 台本の中のパニック");
    }
    #[cfg(test)]
    {
        let r = FORCE_RET.swap(0, Ordering::SeqCst);
        if r != 0 {
            return r - 1;
        }
    }
    // rollback のモードのフックが走査を呼んでいる tick は、フックが置いた指図（feed する入力ワードか描画だけ）に従う。台本・ネットワーク・自動保存・状態の操作には
    // 触らない（フックが持っている入れ物と重なる —— `RB_HELD`）。
    if let Some(r) = rb_tick(&mut io) {
        return r;
    }
    if RB_HELD.load(Ordering::Acquire) != 0 {
        // フックが入れ物を持っているのに指図が無い（来ない見込み）。0 を返すと本物のキーボードが rollback の tick に入ってしまうので、
        // 1 = poll_all を呼ばずに戻る。数えて本ログに出す。
        RB_STRAY.fetch_add(1, Ordering::Relaxed);
        return 1;
    }
    let r = route(&mut io, k);
    state_tick(&mut io, k);
    r
}

/// rollback のモードのフックが置いた指図を取って feed する（指図が無ければ `None` —— 今までの経路）。feed はローカル入力を読まない
/// （自分の入力ワードはフックが 1 フレームに 1 回だけ読む）。進め直しの回は台本の時計（`TICKS`）を進めない。
fn rb_tick(io: &mut RealTickIo) -> Option<u32> {
    if !crate::rollback_play_win::armed() {
        return None;
    }
    let r = crate::rollback_play_win::input_hook(&mut |p1, p2| {
        for (i, w) in [p1, p2, p1 | p2].into_iter().enumerate() {
            io.feed(i as u32, u32::from(w));
        }
    })?;
    if !crate::rollback_play_win::resim_active() {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    Some(r)
}

/// 台本のマッチの状態の操作を書く（どの経路の tick でも、入口へ戻る直前に 1 回）。台本の入れ物が空・分岐が閉じた後は何もしない。
fn state_tick(io: &mut RealTickIo, k: u32) {
    // この tick の中で台本が終わって分岐を閉じたなら、入れ物に触らない（閉じた後は読み取りのスレッドが `arm` で置き直しうる）。
    // 1 と読めれば、閉じるのはゲームのスレッド（ここ）だけなので、この関数の間は置き直されない。
    if SCRIPT_ON.load(Ordering::Acquire) == 0 {
        return;
    }
    // SAFETY: `SCRIPT_ON` を Acquire で 1 と読んだので、`arm` が置いた入れ物が見える。ここはゲームのスレッドで、`route` の中の借用は
    // もう終わっている。
    let Some(engine) = (unsafe { (*ENGINE.0.get()).as_mut() }) else {
        return;
    };
    let slots = [
        STATE_SLOTS[0].load(Ordering::Relaxed),
        STATE_SLOTS[1].load(Ordering::Relaxed),
    ];
    // ネットワーク（lockstep）と巻き戻しの実験の実行では書かずに断る（`state_ops` の先頭の注）。
    let refuse = if NET_ON.load(Ordering::Acquire) != 0 {
        Some(Refuse::Net)
    } else if crate::rollback_win::armed() {
        Some(Refuse::Rollback)
    } else {
        None
    };
    state_ops::apply(engine, io, slots, k, &STATE_LOG, refuse);
    // 段階の `type=` / `bs=` / `enter=`: ルーム名の入力欄へ文字を入れる（ウィンドウから打たれた文字と同じ入力欄。入力欄が閉じていれば入らない。OS の入力は
    // 注がない）。次の tick にメニューが取る。
    if let Some(t) = engine.take_typed() {
        let n = t.unit_count();
        let pushed = crate::text_input_win::push_script((0..n).filter_map(|i| t.unit(i)));
        TYPED_PUSHED.fetch_add(pushed as u32, Ordering::Relaxed);
        TYPED_DROPPED.fetch_add(n.saturating_sub(pushed) as u32, Ordering::Relaxed);
    }
}

// ---- ネットワークの対戦の「リプレイを保存して終了」で保存の画面を飛ばす ----
//
// ネットワーク（`NetPlay::take_save_quit`）が「保存して終了」で閉じた tick に、自動保存がそのマッチで保存関数を呼んでいて
// （`replay_save_win::saved_this_seg`）、分岐 `SAVE_SKIP_SITE` の 9 バイトが合えば、グローバル状態語の `CONTINUED_BIT` を立てる。
// ゲームは画面 15 の最初のフレームでそれを見てタイトルへ移る。立てた印は、画面 14 / 15 を通った後のタイトルか、次に場面 2 へ入った tick に
// 落とす（入力の入口の始まりで毎 tick 見る）。ゲームのスレッドで読み書きし、確保しない・待たない。判断は `higanr_core::save_skip`。
// 観戦者の経路も同じ印を立てる（観戦者のゲームはホストの押下を辿って同じ「保存して終了」を決め、保存の画面が出てしまうため。
// 自動保存の成否に依らない）。

/// 見張りの状態（`save_skip::Watch` の番号 `watch_code`）。書くのも読むのもゲームのスレッドだけ（パニックの受けも同じスレッド）。
static SKIP_WATCH: AtomicU32 = AtomicU32::new(0);
/// 分岐のアドレス（検査は自分の静的な配列へ向ける）。
static SKIP_SITE_AT: AtomicU32 = AtomicU32::new(SAVE_SKIP_SITE.addr);
/// 数え（本ログの「ネットワークの外」の 1 行。ゲームのスレッドが書き、読み取りのスレッドが読む）: 立てた・もう立っていた・断った
/// （`SkipRefuse::ALL` の順）・落とした（タイトル・次のマッチ）・落とせずに次の tick へ回した。
static SKIP_SET: AtomicU32 = AtomicU32::new(0);
static SKIP_ALREADY: AtomicU32 = AtomicU32::new(0);
static SKIP_REFUSED: [AtomicU32; SkipRefuse::ALL.len()] =
    [const { AtomicU32::new(0) }; SkipRefuse::ALL.len()];
static SKIP_CLEARED: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];
static SKIP_CLEAR_RETRY: AtomicU32 = AtomicU32::new(0);

fn watch_code(w: Watch) -> u32 {
    match w {
        Watch::Idle => 0,
        Watch::Leaving => 1,
        Watch::Away => 2,
        Watch::Passed => 3,
    }
}

fn watch_of(c: u32) -> Watch {
    match c {
        1 => Watch::Leaving,
        2 => Watch::Away,
        3 => Watch::Passed,
        _ => Watch::Idle,
    }
}

/// 立てる・落とすのに使う自分のプロセスのメモリ。読みは落ちない読み方（分岐は `read_own_into`、語は `safe_u32` と同じ
/// `ReadProcessMemory`）、書きは `write_own_u32`。検査では語の読み書きを `fake_mem` の表へ向ける。
struct SkipOwn {
    state: u32,
}

impl SkipMem for SkipOwn {
    fn read_site(&mut self, out: &mut [u8]) -> bool {
        crate::win::read_own_into(SKIP_SITE_AT.load(Ordering::Relaxed), out)
    }
    fn read_state(&mut self) -> Option<u32> {
        #[cfg(test)]
        if let Some(v) = fake_mem::get(self.state) {
            return Some(v);
        }
        OwnProcess.u32_at(self.state)
    }
    fn write_state(&mut self, v: u32) -> bool {
        #[cfg(test)]
        if fake_mem::on() {
            fake_mem::set(self.state, v);
            return true;
        }
        write_own_u32(self.state, v)
    }
}

fn skip_own() -> SkipOwn {
    SkipOwn {
        state: load_addrs().global_state,
    }
}

/// ネットワークが「保存して終了」で閉じた tick に 1 回（`net_tick`）。`saved` はこのマッチで自動保存が保存関数を呼んだか。
fn save_skip_mark(saved: bool) -> Marked {
    let mut own = skip_own();
    // 書く語のアドレス（`arm` が置いた `global_state`）を、照合する 9 バイトの `testb` に埋まったアドレスと結び付ける
    // （合わなければ `mark` が書かずに断る）。
    let at = own.state;
    let m = save_skip::mark(&mut own, saved, SAVE_SKIP_SITE.expected, at, CONTINUED_BIT);
    match m {
        Marked::Set => {
            SKIP_SET.fetch_add(1, Ordering::Relaxed);
        }
        Marked::AlreadySet => {
            SKIP_ALREADY.fetch_add(1, Ordering::Relaxed);
        }
        Marked::Refused(r) => {
            if let Some(i) = SkipRefuse::ALL.iter().position(|&x| x == r) {
                SKIP_REFUSED[i].fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    SKIP_WATCH.store(watch_code(Watch::after(m)), Ordering::Relaxed);
    m
}

/// ネットワークの経路の 1 tick（`net_tick`）: マッチの終わりのメニューの「リプレイを保存して終了」でネットワークが閉じた tick なら、このマッチの自動保存が保存関数を
/// 呼んだときだけ印を立てる。この tick の始まりで自動保存の `on_tick` が済んでいる（`route`）。配線を検査で縛るために関数へ出した
/// （`saved_this_seg()` の代わりに `true` を渡すと、自動保存の無い実行でも保存の画面を飛ばし、リプレイが 1 本も残らない）。
fn save_quit_tick(np: &mut NetPlay) {
    if np.take_save_quit() {
        save_skip_mark(save_skip::may_skip(
            SkipSide::Player,
            crate::replay_save_win::saved_this_seg(),
        ));
    }
}

/// 観戦者の経路の 1 tick（`watch_tick`）: 観戦者のゲームがホストの押下を辿って「リプレイを保存して終了」でマッチを出た tick なら、保存の画面を
/// 飛ばす印を立てる。対戦者と違い、自動保存が呼んだかに依らない（`save_skip::may_skip`）。
fn watch_save_quit_tick(wp: &mut WatchPlay) {
    if wp.take_save_quit() {
        save_skip_mark(save_skip::may_skip(
            SkipSide::Watcher,
            crate::replay_save_win::saved_this_seg(),
        ));
    }
}

/// 入力の入口の始まりで毎 tick（`route`）。見張っていなければ原子変数を 1 つ読んで戻る。見張っていれば、今の場面と（メニューなら）
/// MainMenu の画面を読み、落とす tick なら落とす。
fn save_skip_tick(io: &mut RealTickIo) {
    if watch_of(SKIP_WATCH.load(Ordering::Relaxed)) == Watch::Idle {
        return;
    }
    let a = load_addrs();
    let scene = TickIo::data_u32(io, a.supervisor.wrapping_add(SUP_SCENE_CUR));
    // MainMenu のポインタは解放の後も 0 に戻らないので、落ちない読み方でたどる（`scripted::read_obs` と同じ）。
    let screen = if scene == scripted::SCENE_MENU {
        io.safe_u32(a.main_menu_ptr)
            .filter(|&p| p != 0)
            .and_then(|p| p.checked_add(MENU_SCREEN_ID))
            .and_then(|at| io.safe_u32(at))
    } else {
        None
    };
    save_skip_step(scene == netplay::SCENE_MATCH, screen);
}

fn save_skip_step(in_match: bool, screen: Option<u32>) {
    let (next, why) = watch_of(SKIP_WATCH.load(Ordering::Relaxed)).step(in_match, screen);
    let Some(why) = why else {
        SKIP_WATCH.store(watch_code(next), Ordering::Relaxed);
        return;
    };
    if save_skip::clear(&mut skip_own(), CONTINUED_BIT) {
        SKIP_WATCH.store(watch_code(Watch::Idle), Ordering::Relaxed);
        let i = match why {
            Clear::Title => 0,
            Clear::MatchAgain => 1,
        };
        SKIP_CLEARED[i].fetch_add(1, Ordering::Relaxed);
    } else {
        // 読めない・書けない（来ない見込み）。見張りを残し、次の tick にやり直す。
        SKIP_WATCH.store(watch_code(next), Ordering::Relaxed);
        SKIP_CLEAR_RETRY.fetch_add(1, Ordering::Relaxed);
    }
}

/// 入力の入口でパニックを受けたとき（`script_on_tick`）: 立てた印が残っていれば落とす（以後は入口が開かず、見張れない）。
fn save_skip_abandon() {
    if watch_of(SKIP_WATCH.swap(0, Ordering::Relaxed)) != Watch::Idle {
        let _ = save_skip::clear(&mut skip_own(), CONTINUED_BIT);
    }
}

/// 本ログの「ネットワークの外」の 1 行（何も起きていなければ空）。
fn save_skip_text() -> String {
    let l = |a: &AtomicU32| a.load(Ordering::Relaxed);
    let set = l(&SKIP_SET);
    let already = l(&SKIP_ALREADY);
    let refused: Vec<String> = SkipRefuse::ALL
        .iter()
        .zip(SKIP_REFUSED.iter())
        .filter(|(_, n)| l(n) != 0)
        .map(|(r, n)| format!("{} {} 回", r.text(), l(n)))
        .collect();
    if set == 0 && already == 0 && refused.is_empty() {
        return String::new();
    }
    format!(
        "\n保存して終了（保存の画面を飛ばす印）: 立てた {set} 回 / もう立っていた {already} 回 / 立てなかった {} / 落とした（タイトル {} 回・次の試合 {} 回）/ 落とせず次の tick へ {} 回 / いま見張り中 {}",
        if refused.is_empty() {
            "0 回".to_string()
        } else {
            refused.join("・")
        },
        l(&SKIP_CLEARED[0]),
        l(&SKIP_CLEARED[1]),
        l(&SKIP_CLEAR_RETRY),
        if watch_of(l(&SKIP_WATCH)) == Watch::Idle {
            "いいえ"
        } else {
            "はい"
        },
    )
}

/// 自分のプロセスへ 4 バイトを落ちない書き方で書く（アドレスが確保されていない・書けないなら false。例外は出ない）。
fn write_own_u32(addr: u32, v: u32) -> bool {
    let bytes = v.to_le_bytes();
    let mut n: usize = 0;
    // SAFETY: 自分のプロセスの擬似ハンドルと、書くアドレス・書く元・長さ・書けたバイト数の書く先を渡すだけ。アドレスが確保されていない・
    // 書けないなら、OS が失敗を返す。
    let ok = unsafe {
        WriteProcessMemory(
            GetCurrentProcess(),
            addr as usize as *const core::ffi::c_void,
            bytes.as_ptr() as *const core::ffi::c_void,
            bytes.len(),
            &mut n,
        )
    };
    ok != 0 && n == bytes.len()
}

/// 自動保存へ見せるもの（読むだけ。確保しない）。対戦のセッションならネットワークのセッション、観戦者のセッションなら観戦しているマッチの 1P・2P の名前
/// （まだ受けていなければ `WaitingForNames` —— 名前の無いうちは保存しない）。
fn save_view(slot: Option<&Slot>) -> (Option<&NetPlay>, crate::replay_save::Watching) {
    use crate::replay_save::Watching;
    match slot {
        Some(Slot::Play(s)) => (Some(&s.np), Watching::No),
        Some(Slot::Watch(s)) => (
            None,
            s.wp.names()
                .map_or(Watching::WaitingForNames, Watching::Names),
        ),
        None => (None, Watching::No),
    }
}

/// 入口の 1 tick の経路（ネットワーク・巻き戻し・台本）を選んで進める。戻り値は入口への戻り値。
fn route(io: &mut RealTickIo, k: u32) -> u32 {
    // SAFETY: `SCRIPT_ON` を Acquire で 1 と読んだので（`tick_inner`）、`arm` が置いた入れ物が見える。以後はゲームのスレッド（ここ）だけが触る。
    // 入口を呼ぶゲームのスレッドは 1 本（実機でスレッド ID 1 種類）。
    let engine = unsafe { (*ENGINE.0.get()).as_mut() };
    // 観戦者の早送りの余分な回（`RESIM_TICK`）は台本の時計を進めない（進め直しの回と同じ。`rb_tick`）。
    if !crate::rollback_play_win::resim_active() {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    // ネットワークの対戦の「保存して終了」で立てた印を、ゲームが使い終えたら落とす。ネットワークを手放した後（つなぎ直し）・ネットワークの外の経路でも見るので、
    // ネットワークの経路より前に置く。
    save_skip_tick(io);
    // マッチの終わりのリプレイの自動保存（入っていなければ原子変数を 1 つ読むだけ）。ネットワークが動いていれば、印とフォルダの名前のために
    // ネットワークのセッションを見せる（読むだけ）。
    if crate::replay_save_win::armed() {
        let (np, watching) = if NET_ON.load(Ordering::Acquire) != 0 {
            // SAFETY: `NET_ON` を Acquire で 1 と読んだので、`arm_net` が置いたセッションが見える。触るのはゲームのスレッド（ここ）だけで、
            // この参照は `on_tick` の中で終わる。
            save_view(unsafe { (*NET.0.get()).as_ref() })
        } else {
            (None, crate::replay_save::Watching::No)
        };
        crate::replay_save_win::on_tick(np, watching);
    }
    // ゲーム内のメニューが頼んだ「ネットワークを開く仕事」が終わっていれば、セッションを置く。**置いた tick もそのまま下のネットワークの経路へ流す**（置いた tick だけ
    // ゲーム自身の poll に任せると、決定キーを押したままのとき、画面 6・カーソル 4 の決定が内蔵ネット対戦へ漏れる）。置くのは `UI_ON` の実行の、
    // ネットワークがまだ無い間だけ。
    if UI_ON.load(Ordering::Acquire) != 0 && NET_ON.load(Ordering::Acquire) == 0 {
        poll_pending();
    }
    if NET_ON.load(Ordering::Acquire) != 0 {
        // SAFETY: `NET_ON` を Acquire で 1 と読んだので、`arm_net` / `arm_watch` が置いたセッションが見える。以後はゲームのスレッドだけが触る。
        match unsafe { (*NET.0.get()).as_mut() } {
            Some(Slot::Play(slot)) => {
                let (mut relink, mut spectate) = (false, false);
                let r = net_tick(slot, engine, io, k, &mut relink, &mut spectate);
                if relink {
                    // つながりが終わってマッチの外へ戻った（ゲーム内のメニューの実行だけ）。この tick の戻り値はネットワークの経路の判断のまま、
                    // ネットワークのセッションを手放す（次の tick からメニューだけの経路）。
                    relink_teardown();
                }
                if spectate {
                    // 待っているだけの対戦者のセッションの上から「ルームで観戦」が決まった: セッションを手放し、続けて観戦者のネットワークを開く。
                    request_spectate();
                }
                return r;
            }
            Some(Slot::Watch(slot)) => {
                let (r, follow) = watch_tick(slot, engine, io, k);
                if follow.release {
                    // 観戦が終わり、マッチの外へ戻り、ルームのスレッドが終わりの状態を書き終えた: セッションを手放す（メニューは元の項目へ）。
                    watch_teardown();
                }
                if follow.spectate {
                    request_spectate();
                }
                return r;
            }
            None => {}
        }
    }
    // ゲーム内のメニューだけが動いている実行（ネットワークはまだ開いていない）。
    if UI_ON.load(Ordering::Acquire) != 0 {
        return ui_only_tick(engine, io, k);
    }
    let Some(engine) = engine else {
        SCRIPT_ON.store(0, Ordering::SeqCst);
        return 0;
    };
    // 巻き戻しの実験が入っていれば、台本より先に手順を回す（書き戻した後の進め直しの tick は、記録した入力ワードを feed して 1 を返す —— 台本は
    // 進めない）。
    if crate::rollback_win::armed() {
        let mut feed = |p1: u16, p2: u16| {
            let _ = io.local_input();
            for (i, w) in [p1, p2, p1 | p2].iter().enumerate() {
                io.feed(i as u32, u32::from(*w));
            }
        };
        if let Some(r) = crate::rollback_win::on_tick(&mut feed) {
            return r;
        }
    }
    // 自動保存が入っていれば、台本が終わっても分岐を閉じない（台本の後のマッチの終わりでも保存を呼べるように。台本の後は 0 = 元の入力で
    // 進む）。
    script_tick(engine, io, k, crate::replay_save_win::armed())
}

/// 台本の 1 tick（両方の席を台本が押す。M1d と同じ）。`keep_open` なら、台本が終わっても分岐を閉じない（ネットワークの経路）。
fn script_tick(engine: &mut Engine, io: &mut RealTickIo, k: u32, keep_open: bool) -> u32 {
    if !matches!(engine.status(), Status::Running { .. }) {
        return 0;
    }
    let stepped = scripted::advance(engine, io, &load_addrs());
    script_commit(io, k, &stepped, keep_open, true)
}

/// 進めた台本の結果（`scripted::advance`）を feed して、入口への戻り値にする。`place_shot` は撮影の要求を出すか（観戦者の経路は
/// 進めた tick に `game.script_step` で出し終えているので false）。
fn script_commit(
    io: &mut RealTickIo,
    k: u32,
    stepped: &scripted::Stepped,
    keep_open: bool,
    place_shot: bool,
) -> u32 {
    let out = scripted::commit(io, &load_addrs(), k, stepped);
    if place_shot && out.shot != 0 {
        shot_win::place_request(out.shot, out.step, k);
    }
    if let Some(r) = &out.record {
        FEED_RING.push(r);
        FED.fetch_add(1, Ordering::Relaxed);
        crate::rollback_win::after_feed(r.p1, r.p2);
    }
    publish(out.status);
    if out.stop && !keep_open {
        SCRIPT_ON.store(0, Ordering::Release);
    }
    out.ret
}

/// ネットワークの経路の 1 tick（入力の地点の判断 → 入口への戻り値）。`relink` は、つながりが終わってネットワークのセッションを手放してよくなったか（ゲーム内の
/// メニューの実行だけ。`NetPlay::relink_due`。呼び手がセッションの借用を終えてから `relink_teardown` を呼ぶ）。
fn net_tick(
    slot: &mut NetSlot,
    mut engine: Option<&mut Engine>,
    io: &mut RealTickIo,
    k: u32,
    relink: &mut bool,
    spectate: &mut bool,
) -> u32 {
    // SAFETY: 引数の無い呼び出し。
    let tid = unsafe { GetCurrentThreadId() };
    let _ = GAME_TID.compare_exchange(0, tid, Ordering::SeqCst, Ordering::SeqCst);
    let (action, step) = {
        let mut game = NetGame::new(io, engine.as_deref_mut(), load_addrs());
        // ゲーム内のメニュー（`code_side = game` の実行だけ）が、画面 6（と、つながった後の画面 7 の取消）で入力を握る。ネットワークの手入れ
        // （受信・セッションづくり・終わりの印）は握る tick も毎 tick 先に行う（`net_ui::net_input`）。
        let ui = game_ui();
        let menu_run = ui.is_some();
        let now = if menu_run { net_udp::now_unix() } else { 0 };
        let (a, out) = net_ui::net_input(ui, &mut game, &mut slot.np, &mut slot.net, now);
        *spectate = spectate_asked(&out);
        if (out.action.is_some() || out.leave)
            && let Some(ui) = game_ui()
        {
            publish_ui_stats(ui);
        }
        // つなぎ直し: ネットワークが終わった後だけ場面を読む（つながっている間・offer / answer の実行は何も足さない —— マッチの中の遅延の増分 0）。
        if menu_run && slot.np.phase() == netplay::Phase::Ended {
            let scenes = netplay::Game::scenes(&mut game);
            let now_ms = (netplay::Net::now_us(&mut slot.net) / 1_000) as u32;
            *relink = slot
                .np
                .relink_due(now_ms, scenes, RELINK_GRACE.load(Ordering::Relaxed));
        }
        (a, game.script_step)
    };
    save_quit_tick(&mut slot.np);
    ui_hud_tick(&mut slot.np);
    let after = if KEEP_AFTER_SCRIPT.load(Ordering::Relaxed) != 0 {
        AfterScript::Keep
    } else {
        AfterScript::Bye
    };
    if let Some(r) = engine
        .as_deref()
        .and_then(|e| net_game::script_end_bye(e.status(), after))
    {
        // 台本が終わった（終わりに BYE。2 回目からは何もしない）。台本が無い実行と `after_script = keep` の最後の段階の後は閉じない
        // （人の入力で続く）。
        slot.np.close(r);
    }
    NET_STATUS.publish(&slot.np);
    if let Some(s) = step {
        // lockstep のフレームで台本を 1 tick 進めた（自分の席の入力ワードに使った）。
        if s.shot != 0 {
            shot_win::place_request(s.shot, s.step, k);
        }
        publish(s.status);
    }
    if let Action::Played(r) = action {
        NET_FRAMES.push(&r, &slot.np.last_timing());
    }
    // 2（描画だけのフレーム）の出口が置かれていることは `arm_net` が確かめた（入口は出口が 0 なら戻りアドレスを替えない）。
    match net_game::entry_return(action) {
        Some(ret) => ret,
        None => match engine {
            Some(e) => script_tick(e, io, k, true),
            None => 0,
        },
    }
}

/// 観戦者の経路の tick の後に呼び手（`route`）がすること。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WatchFollow {
    /// 観戦者のセッションを手放す（観戦が終わり、マッチの外へ戻り、ルームのスレッドが最後の状態を書き終えた）。
    release: bool,
    /// 「ルームで観戦」が決まった（`UiOut::spectate_open`）。
    spectate: bool,
}

/// 観戦者の経路の 1 tick（`net_tick` の観戦者バージョン）: 台本が終わってもネットワークは閉じない（観戦者の台本はタイトルから 7 の画面までを進め、以後は
/// 記録の列がゲームを動かす）。台本は**繋がっている間も 1 tick に 1 回進む**（`WatchPlay` の Esc の読みが進める）: 観戦中の台本の入力ワードは
/// ゲームへ入れず、ESC ビットだけが観戦をやめる読みに効く。繋がる前・タイトルなど観戦者のゲーム自身の入力で進む tick は、台本と撮影を
/// `net_tick` と同じに進める。
///
/// ゲーム内のメニューを置いた実行（「ルームで観戦」）は、画面 6・カーソル 4 の決定を奪う入口の判定・観戦の待ちの間の項目・左上の行・つながった
/// 後の画面 7 までの自動の前進を、観戦者のセッションでも続ける（`net_ui::watch_input`）。続けないと決定が内蔵ネット対戦へ漏れ、入力の地点が
/// 呼ばれなくなる。
fn watch_tick(
    slot: &mut WatchNetSlot,
    mut engine: Option<&mut Engine>,
    io: &mut RealTickIo,
    k: u32,
) -> (u32, WatchFollow) {
    // SAFETY: 引数の無い呼び出し。
    let tid = unsafe { GetCurrentThreadId() };
    let _ = GAME_TID.compare_exchange(0, tid, Ordering::SeqCst, Ordering::SeqCst);
    let mut follow = WatchFollow::default();
    let (action, step, peeked) = {
        let mut game = NetGame::new(io, engine.as_deref_mut(), load_addrs());
        let a = match game_ui() {
            Some(ui) => {
                let (a, out) = net_ui::watch_input(
                    ui,
                    &mut game,
                    &mut slot.wp,
                    &mut slot.net,
                    net_udp::now_unix(),
                );
                follow.spectate = spectate_asked(&out);
                if out.action.is_some() {
                    publish_ui_stats(ui);
                }
                // 終わったセッションは、マッチの外（メニューの場面）へ戻り、ルームのスレッドが終わりの状態を書き終えたら手放す。終わるまでは場面を
                // 読まない（観戦中の tick に足すのは原子変数 1 つの読みだけ）。
                if slot.wp.is_ended() && spectate_join::finished() {
                    let (cur, next) = netplay::Game::scenes(&mut game);
                    follow.release = cur == scripted::SCENE_MENU && next != netplay::SCENE_MATCH;
                }
                a
            }
            None => slot.wp.on_input(&mut game, &mut slot.net),
        };
        (a, game.script_step, game.peeked)
    };
    watch_save_quit_tick(&mut slot.wp);
    WATCH_STATUS.publish(&slot.wp);
    NET_STATUS.publish_watch(&slot.wp);
    if let Some(s) = step {
        if s.shot != 0 {
            shot_win::place_request(s.shot, s.step, k);
        }
        publish(s.status);
    }
    // 観戦者の絵: 台本の段階は画面 6 で終わるので、7 以降は段階と画面の変わり目・マッチの定期で頼む。頼むだけ（撮るのは描画の入口）。
    let (phase, screen) = (slot.wp.phase(), slot.wp.menu_screen());
    let reason = slot.shots.tick(phase, screen);
    if reason != 0 {
        shot_win::place_request(
            reason,
            crate::spectate_watch::shot_step_code(phase, screen),
            k,
        );
    }
    if let Action::Played(r) = action {
        NET_FRAMES.push(&r, &slot.wp.last_timing());
    }
    (watch_return(action, engine, io, k, peeked), follow)
}

/// 観戦者の経路の戻り値。`peeked` は、繋がっている間の Esc の読みが進めた台本の結果。`Local` 以外（記録の列がゲームを動かす tick）は、台本は
/// 進んでもその入力ワードをゲームへ入れない。`Local` は、読みで進めていればその結果を feed し（二重に進めない）、進めていなければ（繋がる前・
/// 終わった後）ここで台本を進める。
fn watch_return(
    action: Action,
    engine: Option<&mut Engine>,
    io: &mut RealTickIo,
    k: u32,
    peeked: Option<scripted::Stepped>,
) -> u32 {
    match net_game::entry_return(action) {
        Some(ret) => ret,
        None => match (engine, peeked) {
            (Some(_), Some(s)) => script_commit(io, k, &s, true, false),
            (Some(e), None) => script_tick(e, io, k, true),
            (None, _) => 0,
        },
    }
}

/// 開始のフック（`stub::match_start_entry`）が、`NET_ON` が 0 でないときに呼ぶ。パニックは受けて外へ出さない（受けたら
/// ネットワークを止める。入力の経路は台本だけで続く）。
pub extern "C" fn match_start_on() {
    #[cfg(test)]
    MATCH_ON_CALLS.fetch_add(1, Ordering::SeqCst);
    let r = catch_unwind(AssertUnwindSafe(|| {
        if NET_ON.load(Ordering::Acquire) == 0 {
            return;
        }
        if RB_HELD.load(Ordering::Acquire) != 0 {
            // rollback のモードのフックがネットワークのセッションを持っている（走査の中から来た）。触らない（来ない見込み）。
            MATCH_NESTED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // SAFETY: 引数の無い呼び出し。
        let tid = unsafe { GetCurrentThreadId() };
        if GAME_TID.load(Ordering::SeqCst) != tid {
            // 入力の入口を通ったゲームのスレッドでない（か、まだ 1 度も入力の入口を通っていない）。触らない。
            MATCH_FOREIGN.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(mut io) = real_io() else {
            return;
        };
        // SAFETY: `NET_ON` を Acquire で 1 と読み、ここはゲームのスレッド（`GAME_TID` と同じ）。入力の入口の中から呼ばれることは無い
        // （開始のフックは GameManager の更新の中、入力の入口は Supervisor の on_tick の始まり）。
        match unsafe { (*NET.0.get()).as_mut() } {
            Some(Slot::Play(slot)) => {
                let mut game = NetGame::new(&mut io, None, load_addrs());
                slot.np.on_match_start(&mut game);
                NET_STATUS.publish(&slot.np);
            }
            Some(Slot::Watch(slot)) => {
                let mut game = NetGame::new(&mut io, None, load_addrs());
                slot.wp.on_match_start(&mut game);
                WATCH_STATUS.publish(&slot.wp);
                NET_STATUS.publish_watch(&slot.wp);
            }
            None => {}
        }
    }));
    if r.is_err() {
        NET_ON.store(0, Ordering::SeqCst);
        NET_PANICKED.store(1, Ordering::SeqCst);
    }
}

// ---- rollback のモードのフック（走査の包み・Present の後）----

/// フックがネットワークのセッションと台本の入れ物を持っている間 `RB_HELD` を 1 にする（入れ子の入力の入口と開始のフックが入れ物に触らないための旗）。
struct HeldGuard;

impl HeldGuard {
    fn new() -> Self {
        RB_HELD.store(1, Ordering::SeqCst);
        HeldGuard
    }
}

impl Drop for HeldGuard {
    fn drop(&mut self) {
        RB_HELD.store(0, Ordering::SeqCst);
    }
}

/// ネットワークのセッション（対戦）を借りる。ゲームのスレッドから、rollback の入口を書けた実行のときだけ。観戦者のセッション・ネットワークが動いていない実行は `None`。
fn rb_slot() -> Option<&'static mut NetSlot> {
    if NET_ON.load(Ordering::Acquire) == 0 || !crate::rollback_play_win::armed() {
        return None;
    }
    // SAFETY: 引数の無い呼び出し。
    let tid = unsafe { GetCurrentThreadId() };
    let _ = GAME_TID.compare_exchange(0, tid, Ordering::SeqCst, Ordering::SeqCst);
    if GAME_TID.load(Ordering::SeqCst) != tid {
        return None;
    }
    // SAFETY: `NET_ON` を Acquire で 1 と読んだので、`arm_net` が置いたセッションが見える。触るのはゲームのスレッドだけで、このフックは走査の包みと
    // Present の後から順に呼ばれ、入れ子にならない。走査の中から来る入力の入口と開始のフックは、フックが持っている間（`RB_HELD`）は入れ物に触らない。
    match unsafe { (*NET.0.get()).as_mut() } {
        Some(Slot::Play(s)) => Some(&mut **s),
        Some(Slot::Watch(_)) | None => None,
    }
}

/// 先頭のフックが触ってよいネットワークのセッション（対戦）。ネットワークが動いていない・観戦者・rollback のフックが入れ物を持っている間・ゲームのスレッドでないなら `None`。
/// 検査が入れ物に触る条件だけを見られるよう `head_wait_hook` から分けてある。
fn head_wait_slot() -> Option<&'static mut NetSlot> {
    if NET_ON.load(Ordering::Acquire) == 0 || RB_HELD.load(Ordering::Acquire) != 0 {
        return None;
    }
    // SAFETY: 引数の無い呼び出し。
    let tid = unsafe { GetCurrentThreadId() };
    if GAME_TID.load(Ordering::SeqCst) != tid {
        return None;
    }
    // SAFETY: `NET_ON` を Acquire で 1 と読んだので、`arm_net` が置いたセッションが見える。触るのはゲームのスレッドだけで、先頭のフックは do_frame の最初
    // （入力の入口・走査の包み・開始のフックの外）なのでどの借用とも入れ子にならない。
    match unsafe { (*NET.0.get()).as_mut() } {
        Some(Slot::Play(s)) => Some(&mut **s),
        Some(Slot::Watch(_)) | None => None,
    }
}

/// フレームの先頭のフック（`pacing_win::set_head_wait`）。delay 方式の lockstep のとき、自分の入力を読む前に、この tick で要る相手の入力ワードが届くまで
/// 待つ（`NetPlay::head_wait`）。`pace_head`（do_frame の先頭 `0x0042DC44` の入口）が、格子の待ちの後と、全画面のゲーム自身の時計の経路で
/// ゲームがその回にフレームを始める見込みのときにだけ呼ぶ。
pub fn head_wait_hook() {
    if let Some(slot) = head_wait_slot() {
        let _ = slot.np.head_wait(&mut slot.net);
    }
}

/// 先頭のフックを置く（ネットワークのセッションを動かす `NET_ON = 1` の前後。置いても `NET_ON` が 0 の間は `head_wait_hook` が何もしない）。
fn install_head_wait() {
    crate::pacing_win::set_head_wait(Some(head_wait_hook));
}

/// 観戦者のセッションを借りる（走査の包みのフックから）。観戦者のセッションが無い・ネットワークが動いていない・ゲームのスレッドでないなら `None`。借りたら、セッションを
/// 使い終えてから走査を呼ぶ（走査の中の入力の入口 `watch_tick` が同じセッションを借りる）。
fn watch_slot_for_scan() -> Option<&'static mut WatchNetSlot> {
    if NET_ON.load(Ordering::Acquire) == 0 {
        return None;
    }
    // SAFETY: 引数の無い呼び出し。
    let tid = unsafe { GetCurrentThreadId() };
    if GAME_TID.load(Ordering::SeqCst) != tid {
        return None;
    }
    // SAFETY: `NET_ON` を Acquire で 1 と読んだので、`arm_watch` / `install_slot` が置いたセッションが見える。触るのはゲームのスレッドだけ。
    // この参照は呼び手が入れ物を使い終えるまでしか持たず、走査を呼ぶ間は持たない。
    match unsafe { (*NET.0.get()).as_mut() } {
        Some(Slot::Watch(s)) => Some(&mut **s),
        Some(Slot::Play(_)) | None => None,
    }
}

/// 余分な走査の間だけ `RESIM_TICK` を立てる（`stub::input_wait_entry` が `_ticks.csv`・時刻の記録に積まない。落ちても戻す）。
struct ExtraTick;

impl ExtraTick {
    fn new() -> Self {
        crate::rollback_play_win::RESIM_TICK.store(1, Ordering::Relaxed);
        ExtraTick
    }
}

impl Drop for ExtraTick {
    fn drop(&mut self) {
        crate::rollback_play_win::RESIM_TICK.store(0, Ordering::Relaxed);
    }
}

struct RealFfHost {
    this: u32,
    ascii0: Option<u32>,
}

impl crate::spectate_watch::FfHost for RealFfHost {
    fn scan(&mut self, extra: bool) -> i32 {
        let _extra = extra.then(ExtraTick::new);
        crate::rollback_win::call_scan(self.this)
    }

    fn with_wp<R>(&mut self, f: impl FnOnce(&mut WatchPlay, u64) -> R) -> Option<R> {
        let slot = watch_slot_for_scan()?;
        let now = netplay::Net::now_us(&mut slot.net);
        Some(f(&mut slot.wp, now))
    }

    fn begin(&mut self) {
        self.ascii0 = crate::rollback_win::ff_read_ascii();
    }

    fn drop_outputs(&mut self) -> bool {
        self.ascii0
            .is_some_and(crate::rollback_win::ff_drop_outputs)
    }

    fn boundary(&mut self) -> bool {
        // 余分な走査ごとに呼ぶので syscall の無い直読みバージョン（対戦者の経路は変えない）。
        crate::rollback_win::scene_request_pending_fast()
    }
}

/// 観戦者のセッションがあるときの走査の包みのフック: 使っていないマッチのフレームが溜まっている間は走査を余分に回す（`spectate_watch::run_display`）。
/// 走査の call を書いていないときと、セッションが無いときは `None`（普段の 1 回）。走査の call は、rollback の設置か `arm_watch_scan` が書く。
/// rollback の設置が済んでいること（`armed`）は要らない: 早送りが使うのは走査の call・文字の数の欄・`RESIM_TICK` だけ。
fn watch_scan(this: u32) -> Option<i32> {
    if !crate::rollback_win::scan_hooked() {
        return None;
    }
    watch_slot_for_scan()?;
    let mut host = RealFfHost { this, ascii0: None };
    Some(crate::spectate_watch::run_display(&mut host))
}

/// 走査の包み（`0x0042DCA9`）のフック（`rollback_win::set_scan_handler`）。rollback のモードのマッチの間は、ここで自分の入力ワードの取得・送信・
/// 入力の地点（止まっていれば書き戻しと進め直しも）を行い、更新 1 回か描画だけのフレームのために走査を自分で呼ぶ。`Some(ret)` を返すと
/// 包みは do_frame へ `ret` を返し、`None` なら走査を普段どおり 1 回呼ぶ（delay 方式・場面の境目・マッチをやめた後・ネットワークの外）。
pub fn rb_scan_handler(this: u32) -> Option<i32> {
    let Some(slot) = rb_slot() else {
        // 対戦者のセッションが無い。観戦者のセッションがあれば、早送りの経路（遅れて入った観戦者を追いつかせる）。
        return watch_scan(this);
    };
    // 前のフレームの後半でマッチをやめていたら、ここでコピーする（やめた後は `active` が false で下の数えのコピーが回らない）。
    rb_note_abort(&slot.np);
    let Some(mut io) = real_io() else {
        if slot.np.rollback_ready() {
            RB_BYPASS.fetch_add(1, Ordering::Relaxed);
        }
        return None;
    };
    // SAFETY: `rb_slot` が `NET_ON` を見て、ここはゲームのスレッド。台本の入れ物は `SCRIPT_ON` が 1 の間はゲームのスレッドだけが触る。
    // 走査の中の入力の入口はフックが持っている間は入れ物に触らない（`RB_HELD`）。
    let mut engine = unsafe { (*ENGINE.0.get()).as_mut() };
    let _held = HeldGuard::new();
    let active = slot.np.rollback_ready();
    if active && crate::replay_save_win::armed() {
        // マッチの終わりのリプレイの自動保存（delay 方式では走査の中の入力の入口が毎 tick 呼ぶ。rollback のモードの tick は入口が入れ物に触れない
        // ので、フレームに 1 回ここで呼ぶ）。
        crate::replay_save_win::on_tick(Some(&slot.np), crate::replay_save::Watching::No);
    }
    // 台本の撮影の頼みが結び付く tick の番号（フックが呼ぶ走査の入力の入口が取る番号 = 次の番号）。
    let k = stub::next_seq();
    let mut script_step = None;
    let out = crate::rollback_play_win::with_play(|st, mem| {
        let mut game = NetGame::new(&mut io, engine.as_deref_mut(), load_addrs());
        let r = slot
            .np
            .rollback_scan(&mut game, &mut slot.net, st, mem, &mut |rec, t| {
                NET_FRAMES.push(rec, t);
            });
        script_step = game.script_step;
        r
    });
    let Some(out) = out else {
        if active {
            RB_BYPASS.fetch_add(1, Ordering::Relaxed);
        }
        return None;
    };
    rb_note_abort(&slot.np);
    if active {
        let after = if KEEP_AFTER_SCRIPT.load(Ordering::Relaxed) != 0 {
            AfterScript::Keep
        } else {
            AfterScript::Bye
        };
        if let Some(r) = engine
            .as_deref()
            .and_then(|e| net_game::script_end_bye(e.status(), after))
        {
            // 台本が終わった（`net_tick` と同じ。2 回目からは何もしない）。
            slot.np.close(r);
        }
        NET_STATUS.publish(&slot.np);
        ui_hud_tick(&mut slot.np);
        if let Some(s) = script_step {
            if s.shot != 0 {
                shot_win::place_request(s.shot, s.step, k);
            }
            publish(s.status);
        }
        let n = RB_FRAMES.fetch_add(1, Ordering::Relaxed);
        if n.is_multiple_of(64) || out == netplay::RbScan::Pass {
            crate::rollback_play_win::publish_summary(slot.np.rollback_core_stats());
        }
    }
    match out {
        netplay::RbScan::Ran { ret } => Some(ret),
        netplay::RbScan::Pass => None,
    }
}

/// Present の後のフック（`pacing_win::set_after_present`）。rollback のモードのマッチの間だけ、コピーする / 届いた入力ワードで確定の前線を進める。
/// 進め直しは更新を何度も走らせるので、x87 の制御語をゲームの値にして行う（`rollback_play_win::with_game_fpu`）。
pub fn rb_post_present() {
    let Some(slot) = rb_slot() else {
        return;
    };
    if !slot.np.rollback_ready() {
        return;
    }
    let _held = HeldGuard::new();
    let _ = crate::rollback_play_win::with_play(|st, mem| {
        crate::rollback_play_win::with_game_fpu(|| {
            slot.np
                .rollback_post_present(&mut slot.net, st, mem, &mut |rec, t| {
                    NET_FRAMES.push(rec, t);
                })
        })
    });
    rb_note_abort(&slot.np);
}

/// 走査の包みのフックと Present の後のフックを置く（`arm_rollback` が入口を全部書けた後に 1 回）。フックは `NET_ON` が 1 になるまで何もしない。
fn install_rollback_wiring() {
    crate::rollback_win::set_scan_handler(Some(rb_scan_handler));
    crate::pacing_win::set_after_present(Some(rb_post_present));
}

/// 観戦者のネットワークを開くときに、走査の call だけを書いてフックを置く（観戦者の早送り）。観戦者がどの方式のマッチを見るかは繋がるまで分からず、
/// 早送りは方式に依らないので、マッチの方式に関わらず置く。**対戦者の実行からは呼ばない**（delay 方式の対戦者は 1 バイトも書かない約束）。
/// Present の後のフックは置かない（観戦者は巻き戻さない）。書いた（か、既に書いてあった）ら true。
pub fn arm_watch_scan(
    hook_env: &mut dyn crate::hook::HookEnv,
    forbidden: &[higanr_core::patch::AddrRange],
    out: &mut dyn std::io::Write,
) -> bool {
    let ok = crate::rollback_play_win::arm_watch_scan(hook_env, forbidden, out);
    if ok {
        // フックは走査の call を書いた後に置く（フックが先でも、呼ばれるのは call を書いた後）。観戦者のセッションが無い間は何もしない（`watch_scan` が `None`）。
        crate::rollback_win::set_scan_handler(Some(rb_scan_handler));
    }
    ok
}

/// 「ルームで観戦」で観戦者のネットワークを開く（開くスレッド）ときの `arm_watch_scan`。止めて書くのは `RealHook`（呼んだスレッド以外を止め、ゲームの
/// 60 Hz のスレッドが `0x0042DCA9` の命令の途中でない瞬間に 5 バイトの命令の rel32 を書き換えて戻す。メニューの間の 1 回きりで、マッチの中では
/// ない）。本ログ向けの 1 行を返す。
fn arm_watch_scan_at_open() -> String {
    arm_watch_scan_at_open_with(&mut crate::hook_win::RealHook::new())
}

/// `arm_watch_scan_at_open` の本体（止めて書く環境を受ける。検査は偽の命令の表を渡す）。
fn arm_watch_scan_at_open_with(env: &mut dyn crate::hook::HookEnv) -> String {
    if crate::rollback_win::scan_hooked() {
        return "走査の call は書いてある（早送りが効く）".to_string();
    }
    let mut out: Vec<u8> = Vec::new();
    let ok = arm_watch_scan(env, crate::th09_addr::FORBIDDEN, &mut out);
    let detail = String::from_utf8_lossy(&out)
        .lines()
        .collect::<Vec<_>>()
        .join(" / ");
    format!(
        "走査の call を{}（{detail}）",
        if ok {
            "書いた"
        } else {
            "書けなかった"
        }
    )
}

// ---- 台本のファイル（読み取りのスレッド）----

/// DLL のフォルダの台本の名前。
pub const SCRIPT_FILE: &str = "script.txt";
/// 移した先の名前の尻（本ログの名前の先頭に付ける）。
pub const SCRIPT_SUFFIX: &str = "_script.txt";

/// rollback の設置でゲームのスレッドの ID を tick の記録から待つ上限（ms）。ゲームは起動の読み込みの後に最初の tick を打つ。待つのは
/// 読み取りのスレッドだけで、ゲームは止まらない。見つからなければ入れず、delay 方式を名乗る。
const RB_TID_WAIT_MS: u64 = 15_000;

pub struct RealScriptHost {
    pub dll_dir: Option<PathBuf>,
    pub run_path: Option<PathBuf>,
    /// ゲームのフォルダ（exe のフォルダ）。ここと同じかその下のファイルには触らない。
    pub exe_dir: Option<PathBuf>,
}

impl RealScriptHost {
    fn guard(&self, dir: &Path) -> Result<(), String> {
        let exe = self
            .exe_dir
            .as_ref()
            .ok_or("exe のフォルダが分からないので、ゲームのフォルダの外か確かめられない")?;
        if is_same_or_inside(&dir.to_string_lossy(), &exe.to_string_lossy()) {
            return Err(format!(
                "{} が exe のフォルダの中なので触らない",
                dir.display()
            ));
        }
        Ok(())
    }

    /// 台本の置き場（ローダーの `--script` が環境の変数で渡したパスか、DLL のフォルダの `script.txt`）。
    fn script_location(&self) -> Result<crate::net_config::Located, String> {
        let env = higanr_core::script::PATH_ENV;
        crate::net_config::locate(
            env,
            std::env::var_os(env).as_deref(),
            self.dll_dir.as_deref(),
            SCRIPT_FILE,
        )
    }

    /// 対戦者の rollback の設置（`ScriptHost::arm_rollback` の本体。観戦者の走査の call はその呼び手が足す）。
    fn arm_rollback_play(
        &mut self,
        s: &NetSettings,
        hook_env: &mut dyn crate::detour::DetourEnv,
        forbidden: &[higanr_core::patch::AddrRange],
        out: &mut dyn std::io::Write,
    ) -> bool {
        let wish = install_wish(s);
        if wish != higanr_core::wire::Mode::Rollback {
            return crate::rollback_play_win::arm(wish, 0, hook_env, forbidden, None, out);
        }
        // E1 の実験（`[rollback_probe]`・`[alloc_count]`）が同じ入口を使うので、実験が有効な実行では入れない。
        if let Ok(Some(text)) = self.read_net_ini() {
            let probe = !matches!(crate::rollback_probe::settings_from_text(&text), Ok(None));
            let count = !matches!(crate::alloc_count::settings_from_text(&text), Ok(None));
            if probe || count {
                let _ = writeln!(
                    out,
                    "巻き戻し（製品の経路）: 実験（[rollback_probe] か [alloc_count]）が有効なので入れない。HELLO には delay 方式を載せる"
                );
                return false;
            }
        }
        // ゲームのスレッドの ID を tick の記録から取る（確保の向け先を分けるため）。ゲームが動き出すのを待つ。
        let tid = crate::alloc_win::wait_game_thread_id(RB_TID_WAIT_MS);
        let armed = crate::rollback_play_win::arm(
            wish,
            crate::rollback_play_win::DEFAULT_ARENA_BYTES,
            hook_env,
            forbidden,
            tid,
            out,
        );
        if !armed {
            return false;
        }
        // 被弾音の鳴らし方（ini の `rollback_hit_se`）。マッチの前に 1 回。
        let _ = crate::rollback_play_win::with_play(|st, _| st.set_hit_se(s.rollback_hit_se));
        // Present の後のフックは、ペースの設置が Present の call を書いたときだけ届く。書けていなければ、コピーと進め直しは入力の地点
        // （止まっている間）でしか起きない —— 正しさは変わらず、止まる回数と費用が増える。
        if crate::pacing_win::PRESENT_ORIGINAL.load(Ordering::Acquire) == 0 {
            let _ = writeln!(
                out,
                "巻き戻し（製品の経路）: Present の call が書かれていない（[pacing] が入っていない）ので、Present の後の手順は呼ばれない。進め直しは止まっている間の入力の地点だけになる"
            );
        }
        install_rollback_wiring();
        true
    }

    /// ini の置き場（ローダーの `--ini` が環境の変数で渡したパスか、DLL のフォルダの `HiganRTool.ini`）。ネットワーク・ペース・自動保存・確保の数えの
    /// 全部がここを通る。
    fn ini_location(&self) -> Result<crate::net_config::Located, String> {
        let env = higanr_core::ini::PATH_ENV;
        crate::net_config::locate(
            env,
            std::env::var_os(env).as_deref(),
            self.dll_dir.as_deref(),
            crate::net_config::INI_FILE,
        )
    }

    /// 配るバージョンで ini が無ければ既定を書き出す（起動のとき 1 回。`ini_seed`）。置き場が決まらないなら何もしない
    /// （読む側が同じ理由で断る）。
    pub(crate) fn seed_default_ini(&self, default: Option<&str>) -> crate::ini_seed::Seed {
        match self.ini_location() {
            Ok(loc) => crate::ini_seed::seed_process(&loc, default),
            Err(_) => crate::ini_seed::Seed::Skipped,
        }
    }

    fn source(&self) -> Result<PathBuf, String> {
        let loc = self.script_location()?;
        let dir = loc.path.parent().ok_or("台本のフォルダが分からない")?;
        self.guard(dir)?;
        Ok(loc.path)
    }

    fn dest(&self) -> Result<(PathBuf, String), String> {
        let log = self
            .run_path
            .as_ref()
            .ok_or("本ログの置き場が決まっていない")?;
        let dir = log.parent().ok_or("本ログのフォルダが分からない")?;
        self.guard(dir)?;
        let log_name = log
            .file_name()
            .ok_or("本ログの名前が分からない")?
            .to_string_lossy();
        let name = side_file_name(&log_name, SCRIPT_SUFFIX);
        Ok((dir.join(&name), name))
    }
}

/// 置き場の ini を読む。DLL のフォルダの ini が無ければ `Ok(None)`（ネットワークなしで走る）。ローダーが渡した ini が無いのは渡し方の誤りなので、
/// 「無い ⇒ ネットワークなし」に丸めず、読めないとして返す。`fallback` は DLL のフォルダに ini が無いときに返す中身（配るバージョンで書き出せなかったとき）。
pub(crate) fn read_ini_at(
    loc: &crate::net_config::Located,
    fallback: Option<&str>,
) -> Result<Option<String>, String> {
    let path = &loc.path;
    match std::fs::read(path) {
        Ok(b) => String::from_utf8(b)
            .map(Some)
            .map_err(|_| format!("{} が UTF-8 でない", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && loc.from_env.is_none() => {
            Ok(fallback.map(str::to_string))
        }
        Err(e) => Err(format!("{} を読めない: {e}", path.display())),
    }
}

fn wide(p: &Path) -> Vec<u16> {
    OsStr::new(p)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// `from` を `to` へ移す。`to` が既にあれば移さない（旗 0 は「上書きしない・別のドライブへはコピーしない」）。
pub(crate) fn move_no_replace(from: &Path, to: &Path) -> Result<(), u32> {
    let (f, t) = (wide(from), wide(to));
    // SAFETY: どちらも 0 で終わる UTF-16 の列で、呼び出しの間生きている。旗 0。
    let ok = unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), 0) };
    if ok == 0 {
        // SAFETY: 引数の無い呼び出し。直前の MoveFileExW の結果を指す。
        return Err(unsafe { GetLastError() });
    }
    Ok(())
}

impl ScriptHost for RealScriptHost {
    fn read_script(&mut self) -> Result<Option<Vec<u8>>, String> {
        let src = self.source()?;
        match std::fs::read(&src) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{} を読めない: {e}", src.display())),
        }
    }

    fn move_script(&mut self) -> Result<String, String> {
        // ローダーが渡した台本は移さない（`session::load_script` はこの関数を呼ばない。呼ばれたら断る）。
        if self.script_location()?.from_env.is_some() {
            return Err("ローダーが渡した台本は移さない".to_string());
        }
        let src = self.source()?;
        let (to, name) = self.dest()?;
        move_no_replace(&src, &to).map_err(|code| {
            format!(
                "{} を {} へ移せない（GetLastError = {code}）",
                src.display(),
                to.display()
            )
        })?;
        Ok(name)
    }

    fn prepare_shots(&mut self) -> Result<(), String> {
        shot_win::prepare(D3D8_DEVICE_PTR, D3D8_PRESENT_PARAMS)
    }

    fn arm(&mut self, script: Script, feed_fn: u32, local_fn: u32) -> Result<(), String> {
        arm(script, GameAddrs::th09(), feed_fn, local_fn)
    }

    fn arm_io(&mut self, feed_fn: u32, local_fn: u32) -> Result<(), String> {
        arm_io(GameAddrs::th09(), feed_fn, local_fn)
    }

    fn shots_on(&mut self) {
        shot_win::set_on();
    }

    fn start(&mut self) -> Result<(), String> {
        start()
    }

    fn read_net_ini(&mut self) -> Result<Option<String>, String> {
        // 読むだけ（台本と違って移さない）。
        read_ini_at(&self.ini_location()?, crate::ini_seed::fallback())
    }

    fn ini_origin(&self) -> String {
        match self.ini_location() {
            Ok(l) => l.text(),
            Err(e) => format!("置き場が決まらない（{e}）"),
        }
    }

    fn explicit_script(&self) -> Option<String> {
        match self.script_location() {
            Ok(l) if l.from_env.is_some() => Some(l.text()),
            // 置き場が決まらないなら、先に呼ばれる `read_script` が同じ理由で断っている（ここへは来ない）。
            Ok(_) | Err(_) => None,
        }
    }

    fn arm_net(&mut self, s: &NetSettings, set_state_fn: u32) -> Result<String, String> {
        // 接続コードの置き場（DLL のフォルダの `code\` か ini の `code_dir`）。ゲームのフォルダの中なら開かない。
        let code_dir = match s.code_side {
            None => None,
            Some(_) => {
                let d = net_code::code_dir(self.dll_dir.as_deref(), s.code_dir.as_deref())?;
                self.guard(&d)?;
                Some(d)
            }
        };
        // ゲーム内のメニューの実行は、ネットワークを開かずにメニューだけを置く。観戦用のコードの置き場の確かめ（下）はこの経路では要らない
        // （観戦のホストはルームで配り、ファイルを使わない）ので、この分岐の後ろに置く。
        if s.code_side == Some(CodeSide::Game) {
            let ini_path = self.ini_location().ok().map(|l| l.path);
            return arm_game_ui(s, set_state_fn, code_dir, ini_path);
        }
        // 観戦用のコードの置き場（`[spectate] code_dir`、無ければ `[netplay]` の `code_dir`、無ければ DLL のフォルダの `code`）。
        // 同じくゲームのフォルダの中なら開かない。
        let spectate_dir = match s.spectate.as_ref() {
            None => None,
            Some(sp) => {
                let setting = sp.code_dir.as_deref().or(s.code_dir.as_deref());
                let d = net_code::code_dir(self.dll_dir.as_deref(), setting)?;
                self.guard(&d)?;
                Some(d)
            }
        };
        open_net(s, set_state_fn, code_dir, spectate_dir)
    }

    fn saved_name(&mut self, s: &NetSettings) -> Option<String> {
        // ゲーム内のメニューの実行だけ（覚えるファイルの置き場は接続コードの置き場）。置き場がゲームのフォルダの中なら読まない
        // （`arm_net` と同じガード）。
        if s.code_side != Some(CodeSide::Game) {
            return None;
        }
        let d = net_code::code_dir(self.dll_dir.as_deref(), s.code_dir.as_deref()).ok()?;
        self.guard(&d).ok()?;
        crate::player_name_file::read(&d)
    }

    fn os_langid(&mut self) -> u16 {
        crate::ui_lang_win::os_ui_langid()
    }

    fn start_net(&mut self) -> Result<(), String> {
        start_net()
    }

    fn arm_portrait(&mut self, fns: [u32; 3]) {
        arm_portrait(fns)
    }

    fn arm_poll(&mut self, addr: u32) {
        arm_poll(addr)
    }

    fn arm_rollback(
        &mut self,
        s: &NetSettings,
        hook_env: &mut dyn crate::detour::DetourEnv,
        forbidden: &[higanr_core::patch::AddrRange],
        out: &mut dyn std::io::Write,
    ) -> bool {
        let played = self.arm_rollback_play(s, hook_env, forbidden, out);
        // 観戦者の実行は、マッチの方式に関わらず走査の call を置く（観戦者の早送り）。rollback の設置が済んでいれば何もしない。ゲーム内のメニューの
        // 実行が後から「ルームで観戦」を選ぶ場合は、ネットワークを開くとき（`open_watch_net`）に置く（起動のときは観戦者か対戦者か分からず、
        // 対戦者の delay 方式の実行には 1 バイトも書かない）。
        if s.code_side == Some(CodeSide::Spectate) {
            arm_watch_scan(hook_env, forbidden, out);
        }
        played
    }

    fn alloc_count(
        &mut self,
        step: crate::alloc_count::AllocStep<'_>,
        out: &mut dyn std::io::Write,
    ) {
        crate::alloc_win::on_step(self.read_net_ini(), self.run_path.clone(), step, out)
    }

    fn pacing_host(&mut self) -> Option<Box<dyn crate::pacing::PacingHost>> {
        // ペースと時刻の記録（ini はネットワークと同じ DLL のフォルダの HiganRTool.ini、CSV は本ログの隣）。
        Some(Box::new(crate::pacing_win::RealPacingHost::new(
            self.dll_dir.clone(),
            self.run_path.clone(),
            self.exe_dir.clone(),
        )))
    }

    fn overlay_host(&mut self) -> Option<Box<dyn crate::overlay::OverlayHost>> {
        // 文字の描画（`[overlay]` はネットワークと同じ ini。読むだけ）。見本の PNG は本ログの隣へ（ゲームのフォルダの中には書かない）。
        Some(Box::new(crate::overlay_win::RealOverlayHost::new(
            self.read_net_ini(),
            self.run_path.clone(),
            self.exe_dir.clone(),
        )))
    }

    fn arm_replay(&mut self, net: Option<&NetSettings>) -> Result<Option<String>, String> {
        // `[replay]` はネットワークと同じ ini（DLL のフォルダ）にある。読むだけ。
        let text = self.read_net_ini()?;
        crate::replay_save_win::arm(
            text.as_deref(),
            net,
            self.run_path.as_deref(),
            self.exe_dir.as_deref(),
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! 本物の入力の入口を検査のプロセスで通す: 偽のゲームのメモリ（静的変数）と、偽の feed（fastcall。模型どおりに偽の InputManager を
    //! 動かし、呼ばれた順を残す）と偽のローカル入力。
    use super::*;
    use crate::feeds::{FeedChecker, FeedRead, im_from_bytes, im_to_bytes};
    use crate::netplay::Game;
    use crate::stub::testkit::{
        AFTER, PRE_ESP, REAL_HOOK_TESTS, REG_EBP, REG_EBX, REG_EDI, TARGET_HITS, call_with_regs,
        fake_exit_addr, fake_on_tick_addr, fake_this_addr, load, seen_target_addr, set_fake,
    };
    use crate::stub::{
        next_seq, set_copy_table, set_original_target, set_render_only_exit, set_window,
    };
    use crate::th09_addr::{PLAYER_GAUGE, PLAYER_INV_TIMER_F, PLAYER_INV_TIMER_I, PLAYER_LIFE};
    use core::sync::atomic::AtomicU8;
    use higanr_core::input_model::{InputState, keys};
    use higanr_core::script::parse;

    pub(crate) static FAKE_DATA: [AtomicU32; 6] = [const { AtomicU32::new(0) }; 6];
    pub(crate) static FAKE_IMS: [[AtomicU8; IM_BYTES]; 3] =
        [const { [const { AtomicU8::new(0) }; IM_BYTES] }; 3];
    static FEED_LOG: [AtomicU32; 64] = [const { AtomicU32::new(0) }; 64];
    static FEED_N: AtomicU32 = AtomicU32::new(0);
    static LOCAL_N: AtomicU32 = AtomicU32::new(0);
    const LOCAL_VALUE: u32 = 0xBEEF_0033;

    fn im_state(i: usize) -> InputState {
        let b: [u8; IM_BYTES] = core::array::from_fn(|j| FAKE_IMS[i][j].load(Ordering::SeqCst));
        im_from_bytes(&b)
    }

    fn set_im(i: usize, s: &InputState) {
        for (a, v) in FAKE_IMS[i].iter().zip(im_to_bytes(s)) {
            a.store(v, Ordering::SeqCst);
        }
    }

    extern "fastcall" fn fake_feed(index: u32, word: u32) {
        let n = FEED_N.fetch_add(1, Ordering::SeqCst) as usize;
        if let Some(a) = FEED_LOG.get(n) {
            a.store(index << 16 | (word & 0xFFFF), Ordering::SeqCst);
        }
        if let Some(i) = [0usize, 1, 2].into_iter().find(|&i| i as u32 == index) {
            let mut s = im_state(i);
            s.feed(word as u16);
            set_im(i, &s);
        }
    }

    static LOCAL_OVERRIDE: AtomicU32 = AtomicU32::new(u32::MAX);

    extern "C" fn fake_local() -> u32 {
        LOCAL_N.fetch_add(1, Ordering::SeqCst);
        match LOCAL_OVERRIDE.load(Ordering::SeqCst) {
            u32::MAX => LOCAL_VALUE,
            w => w,
        }
    }

    pub(crate) fn fake_addrs() -> GameAddrs {
        let a = |x: &'static AtomicU32| x as *const AtomicU32 as usize as u32;
        GameAddrs {
            supervisor: fake_this_addr(),
            pause: a(&FAKE_DATA[0]),
            loading: a(&FAKE_DATA[1]),
            global_state: a(&FAKE_DATA[2]),
            main_menu_ptr: a(&FAKE_DATA[3]),
            input_managers: core::array::from_fn(|i| FAKE_IMS[i].as_ptr() as usize as u32),
            match_end_menu: a(&FAKE_DATA[4]),
            gui_ptr: a(&FAKE_DATA[5]),
        }
    }

    pub(crate) fn feed_fn_addr() -> u32 {
        fake_feed as *const () as usize as u32
    }
    pub(crate) fn local_fn_addr() -> u32 {
        fake_local as *const () as usize as u32
    }

    fn prepare_entry(scene: u32) {
        stop();
        shot_win::set_off();
        set_original_target(seen_target_addr());
        set_window(0);
        assert!(set_copy_table(&[]));
        set_render_only_exit(0);
        set_fake(1, scene, scene, 1.0, 2.0);
        for a in FAKE_DATA.iter() {
            a.store(0, Ordering::SeqCst);
        }
        for i in 0..3 {
            set_im(i, &InputState::new());
        }
        FEED_N.store(0, Ordering::SeqCst);
    }

    fn feed_log() -> Vec<(u32, u32)> {
        let n = FEED_N.load(Ordering::SeqCst) as usize;
        FEED_LOG[..n.min(64)]
            .iter()
            .map(|a| {
                let v = a.load(Ordering::SeqCst);
                (v >> 16, v & 0xFFFF)
            })
            .collect()
    }

    #[test]
    fn entry_runs_the_script_through_the_real_hook() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        let script = parse("scene=3 wait=1 p1=Z+UP p2=X hold=2\n").unwrap();
        arm(script, fake_addrs(), feed_fn_addr(), local_fn_addr()).expect("arm");
        start().expect("start");
        let ring0 = FEED_RING.next_seq();
        let local0 = LOCAL_N.load(Ordering::SeqCst);
        let mut ks = Vec::new();
        let want_feed: [&[(u32, u32)]; 5] = [
            &[(0, 0), (1, 0), (2, 0)],
            &[(0, 0x11), (1, 0x02), (2, 0x13)],
            &[(0, 0x11), (1, 0x02), (2, 0x13)],
            &[],
            &[],
        ];
        for (t, want) in want_feed.iter().enumerate() {
            FEED_N.store(0, Ordering::SeqCst);
            let hits0 = TARGET_HITS.load(Ordering::SeqCst);
            let ticks0 = progress().ticks;
            ks.push(next_seq());
            call_with_regs(fake_on_tick_addr());
            assert_eq!(feed_log(), want.to_vec(), "tick {}: feed の順", t + 1);
            let went_orig = TARGET_HITS.load(Ordering::SeqCst) - hits0;
            assert_eq!(
                went_orig,
                u32::from(want.is_empty()),
                "tick {}: 元の飛び先へ行った回数",
                t + 1
            );
            let after = load(&AFTER);
            assert_eq!(
                after[0],
                1,
                "tick {}: fake_on_tick の続きへ戻っていない",
                t + 1
            );
            assert_eq!(after[3], REG_EBX, "tick {}: ebx", t + 1);
            assert_eq!(after[4], fake_this_addr(), "tick {}: esi", t + 1);
            assert_eq!(after[5], REG_EDI, "tick {}: edi", t + 1);
            assert_eq!(after[6], REG_EBP, "tick {}: ebp", t + 1);
            assert_eq!(
                after[7],
                PRE_ESP.load(Ordering::SeqCst),
                "tick {}: esp",
                t + 1
            );
            let called = progress().ticks - ticks0;
            assert_eq!(called, u32::from(t < 4), "tick {}: 台本を呼んだ回数", t + 1);
        }
        assert_eq!(
            LOCAL_N.load(Ordering::SeqCst) - local0,
            3,
            "ローカル入力は押した 3 tick に 1 回ずつ"
        );
        let p = progress();
        assert_eq!((p.state, p.fed, p.on), (RunState::Done, 3, false));
        assert_eq!(SCRIPT_ON.load(Ordering::SeqCst), 0);
        let mut c = FeedChecker::default();
        for (j, seq) in (ring0..FEED_RING.next_seq()).enumerate() {
            let FeedRead::Record(r) = FEED_RING.read(seq) else {
                panic!("feed のリングバッファの {seq} が読めない");
            };
            assert_eq!(r.k, ks[j], "記録の k");
            assert_eq!(r.local, LOCAL_VALUE as u16);
            assert_eq!(c.check(&r).im, [0; 3], "記録 {j}");
        }
        assert_eq!(c.records, 3);
        assert_eq!((c.cont_pairs, c.cont_breaks), (2, 0));
        assert_eq!(im_state(0).cur, keys::Z | keys::UP);
        assert_eq!(im_state(0).cnt[0], 2);
        assert_eq!(im_state(2).cur, 0x13);
    }

    #[test]
    fn entry_routes_each_return_value() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        arm(
            parse("scene=3 wait=100 p1=Z\n").unwrap(),
            fake_addrs(),
            feed_fn_addr(),
            local_fn_addr(),
        )
        .unwrap();
        start().unwrap();
        let cases = [
            (0, fake_exit_addr(), 1, 1),
            (1, fake_exit_addr(), 1, 0),
            (2, fake_exit_addr(), 3, 0),
            (2, 0, 1, 0),
        ];
        for (ret, exit, want_eax, want_hits) in cases {
            set_render_only_exit(exit);
            FORCE_RET.store(ret + 1, Ordering::SeqCst);
            let hits0 = TARGET_HITS.load(Ordering::SeqCst);
            call_with_regs(fake_on_tick_addr());
            let after = load(&AFTER);
            let what = format!("戻り値 {ret}・出口 0x{exit:X}");
            assert_eq!(after[0], want_eax, "{what}: on_tick が返した値");
            assert_eq!(
                TARGET_HITS.load(Ordering::SeqCst) - hits0,
                want_hits,
                "{what}: 元の飛び先"
            );
            assert_eq!(after[3], REG_EBX, "{what}: ebx");
            assert_eq!(after[4], fake_this_addr(), "{what}: esi");
            assert_eq!(after[5], REG_EDI, "{what}: edi");
            assert_eq!(after[6], REG_EBP, "{what}: ebp");
            assert_eq!(after[7], PRE_ESP.load(Ordering::SeqCst), "{what}: esp");
            assert_eq!(
                FORCE_RET.load(Ordering::SeqCst),
                0,
                "{what}: 口を使っていない"
            );
        }
        set_render_only_exit(0);
        stop();
    }

    #[test]
    fn arm_net_refuses_bad_setups() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        let slot = || {
            let peer = std::net::SocketAddr::from(([127, 0, 0, 1], 9));
            let (mut net, th) =
                net_udp::open(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), &[peer]).unwrap();
            let cfg = higanr_core::link::Config::single(
                higanr_core::handshake::Config {
                    session_id: 1,
                    role_nonce: 1,
                    latency: 2,
                    name: higanr_core::handshake::Name::new("t"),
                    host_seat: higanr_core::wire::Seat::P1,
                },
                higanr_core::auth::Secret([1; 16]),
                peer,
                higanr_core::seal::KeySeed([0x4B; 32]),
            );
            let np = NetPlay::new(cfg, netplay::Config::default(), &mut net);
            (NetSlot { np, net }, th)
        };
        FEED_FN.store(0, Ordering::SeqCst);
        // SAFETY: 分岐は閉じている（`prepare_entry`）。検査は REAL_HOOK_TESTS の下で 1 本ずつ。
        unsafe { *ENGINE.0.get() = None };
        let (s, _t) = slot();
        assert!(arm_net(s, 0x1234).unwrap_err().contains("関数のアドレス"));
        arm(
            parse("scene=3 p1=Z\n").unwrap(),
            fake_addrs(),
            feed_fn_addr(),
            local_fn_addr(),
        )
        .unwrap();
        let (s, _t) = slot();
        assert!(arm_net(s, 0).unwrap_err().contains("関数のアドレス"));
        set_render_only_exit(0);
        let (s, _t) = slot();
        assert!(arm_net(s, 0x1234).unwrap_err().contains("出口"));
        set_render_only_exit(crate::stub::testkit::fake_exit_addr());
        start().unwrap();
        let (s, _t) = slot();
        assert!(arm_net(s, 0x1234).unwrap_err().contains("開いている"));
        stop();
        let (s, _t) = slot();
        let e = arm_net_with(s, 0x1234, &[(0x10, 4)]).unwrap_err();
        assert!(e.contains("0x00000010") && e.contains("読めない"), "{e}");
        // SAFETY: 読むだけ。分岐は閉じている。
        assert!(unsafe { (*NET.0.get()).is_none() });
        assert!(start_net().is_err(), "セッションが無いのに動かした");
        assert_eq!(NET_ON.load(Ordering::SeqCst), 0);
        let (s, _t) = slot();
        let here = &FAKE_DATA as *const _ as usize as u32;
        arm_net_with(s, 0x1234, &[(here, 16)]).expect("揃ったのに置かない");
        assert_eq!(SET_STATE_FN.load(Ordering::SeqCst), 0x1234);
        start_net().expect("置いたのに動かせない");
        assert_eq!(NET_ON.load(Ordering::SeqCst), 1);
        let (s, _t) = slot();
        assert!(arm_net_with(s, 0x1234, &[(here, 16)]).is_err());
        NET_ON.store(0, Ordering::SeqCst);
        // SAFETY: 分岐は閉じている。検査は REAL_HOOK_TESTS の下で 1 本ずつ。
        unsafe { *NET.0.get() = None };
        SET_STATE_FN.store(0, Ordering::SeqCst);
        set_render_only_exit(0);
    }

    static PORTRAIT_SEEN: [AtomicU32; 5] = [const { AtomicU32::new(0) }; 5];
    static PORTRAIT_ORDER: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
    static PORTRAIT_N: AtomicU32 = AtomicU32::new(0);

    fn portrait_saw(which: u32, all: [u32; 5]) {
        let n = PORTRAIT_N.fetch_add(1, Ordering::SeqCst) as usize;
        if let Some(a) = PORTRAIT_ORDER.get(n) {
            a.store(which + 1, Ordering::SeqCst);
        }
        for (s, v) in PORTRAIT_SEEN.iter().zip(all) {
            s.store(v, Ordering::SeqCst);
        }
    }

    extern "thiscall" fn fake_portrait_0(m: u32, a: u32, b: u32, c: u32, d: u32) {
        portrait_saw(0, [m, a, b, c, d]);
    }
    extern "thiscall" fn fake_portrait_1(m: u32, a: u32, b: u32, c: u32, d: u32) {
        portrait_saw(1, [m, a, b, c, d]);
    }
    extern "thiscall" fn fake_portrait_2(m: u32, a: u32, b: u32, c: u32, d: u32) {
        portrait_saw(2, [m, a, b, c, d]);
    }

    #[test]
    fn portrait_calls_reach_the_right_function_with_thiscall() {
        use crate::net_portrait::PortraitFn;
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let addrs = [
            fake_portrait_0 as *const () as usize as u32,
            fake_portrait_1 as *const () as usize as u32,
            fake_portrait_2 as *const () as usize as u32,
        ];
        FEED_FN.store(feed_fn_addr(), Ordering::SeqCst);
        LOCAL_FN.store(local_fn_addr(), Ordering::SeqCst);
        arm_portrait([addrs[0], 0, addrs[2]]);
        assert!(!real_io().unwrap().portrait_armed());
        assert_eq!(
            [0, 1, 2].map(|i| PORTRAIT_FNS[i].load(Ordering::SeqCst)),
            [0, 0, 0]
        );
        PORTRAIT_N.store(0, Ordering::SeqCst);
        real_io().unwrap().call_portrait(
            0x0500_0000,
            PortraitCall {
                f: PortraitFn::Slide,
                args: [1, 2, 3, 4],
            },
        );
        assert_eq!(
            PORTRAIT_N.load(Ordering::SeqCst),
            0,
            "照合していないのに呼んだ"
        );
        arm_portrait(addrs);
        let mut io = real_io().unwrap();
        assert!(io.portrait_armed());
        let calls = [
            (PortraitFn::SlideBack, [0x11, 0x5C, 0x10, 2]),
            (PortraitFn::Marker, [1, 0x22, 0x33, 0x004A_1E2C]),
            (PortraitFn::Slide, [0x44, 0x3D, 0x10, 2]),
        ];
        for (f, args) in calls {
            io.call_portrait(0x0500_0000, PortraitCall { f, args });
            let seen = [0, 1, 2, 3, 4].map(|i| PORTRAIT_SEEN[i].load(Ordering::SeqCst));
            assert_eq!(
                seen,
                [0x0500_0000, args[0], args[1], args[2], args[3]],
                "{f:?}"
            );
        }
        let order = [0, 1, 2].map(|i| PORTRAIT_ORDER[i].load(Ordering::SeqCst));
        assert_eq!(
            order,
            [2, 3, 1],
            "SlideBack → 番号 1、Marker → 2、Slide → 0（+1 で記録）"
        );
        io.call_portrait(
            0,
            PortraitCall {
                f: PortraitFn::Slide,
                args: [0; 4],
            },
        );
        assert_eq!(PORTRAIT_N.load(Ordering::SeqCst), 3);
        arm_portrait([0, 0, 0]);
        FEED_FN.store(0, Ordering::SeqCst);
        LOCAL_FN.store(0, Ordering::SeqCst);
    }

    static POLL_N: AtomicU32 = AtomicU32::new(0);
    static POLL_SEAT: AtomicU32 = AtomicU32::new(u32::MAX);
    /// 偽の poll が返す値（上位 16 ビットは捨てられるはず）。
    const POLL_VALUE: u32 = 0xDEAD_1234;

    /// 偽の poll（ゲームの poll と同じ fastcall・引数 1 つ）。本物と同じく InputManager[席] の `IM_BYTES` バイトの全部
    /// （最後の 1 バイトまで）を、元の値と必ず違う値で書き散らす。
    extern "fastcall" fn fake_poll(seat: u32) -> u32 {
        POLL_N.fetch_add(1, Ordering::SeqCst);
        POLL_SEAT.store(seat, Ordering::SeqCst);
        if let Some(im) = FAKE_IMS.get(seat as usize) {
            for (j, a) in im.iter().enumerate() {
                a.store(0xF0 ^ j as u8, Ordering::SeqCst);
            }
        }
        POLL_WORDS
            .get(seat as usize)
            .map_or(POLL_VALUE, |w| w.load(Ordering::SeqCst))
    }

    static POLL_WORDS: [AtomicU32; 3] = [const { AtomicU32::new(POLL_VALUE) }; 3];

    #[test]
    fn local_input_reads_seat_zero_with_poll_and_restores_the_input_manager() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        arm_io(fake_addrs(), feed_fn_addr(), local_fn_addr()).expect("arm_io");
        let init = |i: usize, j: usize| 0x11u8.wrapping_mul(i as u8 + 1).wrapping_add(j as u8);
        let dump = |i: usize| -> Vec<u8> {
            FAKE_IMS[i]
                .iter()
                .map(|a| a.load(Ordering::SeqCst))
                .collect()
        };
        for (i, im) in FAKE_IMS.iter().enumerate() {
            for (j, a) in im.iter().enumerate() {
                a.store(init(i, j), Ordering::SeqCst);
            }
        }
        let want0: Vec<u8> = (0..IM_BYTES).map(|j| init(0, j)).collect();
        for j in 0..IM_BYTES {
            assert_ne!(0xF0 ^ j as u8, init(0, j), "{j} 番目");
        }
        POLL_N.store(0, Ordering::SeqCst);
        let local0 = LOCAL_N.load(Ordering::SeqCst);
        assert_eq!(real_io().unwrap().local_input(), LOCAL_VALUE);
        assert_eq!(LOCAL_N.load(Ordering::SeqCst), local0 + 1);
        assert_eq!(POLL_N.load(Ordering::SeqCst), 0);
        arm_poll(fake_poll as *const () as usize as u32);
        let mut io = real_io().unwrap();
        assert!(io.poll.is_some());
        assert_eq!(io.local_input(), POLL_VALUE & 0xFFFF);
        assert_eq!(POLL_N.load(Ordering::SeqCst), 1);
        assert_eq!(POLL_SEAT.load(Ordering::SeqCst), 0, "席 0 で呼ぶ");
        assert_eq!(
            LOCAL_N.load(Ordering::SeqCst),
            local0 + 1,
            "0x0042BE40 は呼ばない"
        );
        assert_eq!(dump(0), want0, "InputManager[0] を元に戻さない");
        for i in 1..3 {
            let want: Vec<u8> = (0..IM_BYTES).map(|j| init(i, j)).collect();
            assert_eq!(dump(i), want, "InputManager[{i}] が変わった");
        }
        let init2 = |j: usize| 0x5Au8.wrapping_add(3 * j as u8);
        for (j, a) in FAKE_IMS[0].iter().enumerate() {
            a.store(init2(j), Ordering::SeqCst);
        }
        let want0b: Vec<u8> = (0..IM_BYTES).map(init2).collect();
        assert_ne!(want0b, want0);
        assert_eq!(io.local_input(), POLL_VALUE & 0xFFFF);
        assert_eq!(dump(0), want0b, "2 回目に呼ぶ直前の中身へ戻さない");
        arm_io(fake_addrs(), feed_fn_addr(), local_fn_addr()).expect("arm_io");
        assert!(real_io().unwrap().poll.is_none());
        arm_poll(0);
        FEED_FN.store(0, Ordering::SeqCst);
        LOCAL_FN.store(0, Ordering::SeqCst);
    }

    #[test]
    fn a_scriptless_net_runs_through_the_real_hook() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        arm_io(fake_addrs(), feed_fn_addr(), local_fn_addr()).expect("arm_io");
        assert_eq!(progress().state, RunState::Idle);
        assert!(start().is_err(), "台本もネットワークも無いのに分岐を開いた");
        set_render_only_exit(crate::stub::testkit::fake_exit_addr());
        let peer = std::net::SocketAddr::from(([127, 0, 0, 1], 9));
        let (mut net, _th) =
            net_udp::open(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), &[peer]).unwrap();
        let cfg = higanr_core::link::Config::single(
            higanr_core::handshake::Config {
                session_id: 1,
                role_nonce: 1,
                latency: 2,
                name: higanr_core::handshake::Name::new("t"),
                host_seat: higanr_core::wire::Seat::P1,
            },
            higanr_core::auth::Secret([1; 16]),
            peer,
            higanr_core::seal::KeySeed([0x4B; 32]),
        );
        let np = NetPlay::new(cfg, netplay::Config::default(), &mut net);
        let here = &FAKE_DATA as *const _ as usize as u32;
        arm_net_with(NetSlot { np, net }, 0x1234, &[(here, 16)])
            .expect("台本の入れ物が無いとネットワークのセッションを置けない");
        start_net().expect("start_net");
        start().expect("ネットワークが動いているのに分岐を開かない");
        let local0 = LOCAL_N.load(Ordering::SeqCst);
        for t in 0..3 {
            FEED_N.store(0, Ordering::SeqCst);
            let hits0 = TARGET_HITS.load(Ordering::SeqCst);
            call_with_regs(fake_on_tick_addr());
            assert_eq!(
                TARGET_HITS.load(Ordering::SeqCst) - hits0,
                1,
                "tick {t}: 元の飛び先"
            );
            assert!(feed_log().is_empty(), "tick {t}: feed した");
            let after = load(&AFTER);
            assert_eq!(after[3], REG_EBX, "tick {t}: ebx");
            assert_eq!(after[4], fake_this_addr(), "tick {t}: esi");
            assert_eq!(after[5], REG_EDI, "tick {t}: edi");
            assert_eq!(after[6], REG_EBP, "tick {t}: ebp");
            assert_eq!(after[7], PRE_ESP.load(Ordering::SeqCst), "tick {t}: esp");
        }
        assert_eq!(
            NET_STATUS.snapshot().ticks,
            3,
            "ネットワークの判断を呼んでいない"
        );
        assert_eq!(
            LOCAL_N.load(Ordering::SeqCst),
            local0,
            "繋がる前にローカル入力を読んだ"
        );
        assert_eq!(
            SCRIPT_ON.load(Ordering::SeqCst),
            1,
            "台本が無いだけで閉じた"
        );
        NET_ON.store(0, Ordering::SeqCst);
        let hits0 = TARGET_HITS.load(Ordering::SeqCst);
        call_with_regs(fake_on_tick_addr());
        assert_eq!(TARGET_HITS.load(Ordering::SeqCst) - hits0, 1);
        assert_eq!(
            SCRIPT_ON.load(Ordering::SeqCst),
            0,
            "台本もネットワークも無いのに開いたまま"
        );
        stop();
        // SAFETY: 分岐は閉じている。検査は REAL_HOOK_TESTS の下で 1 本ずつ。
        unsafe { *NET.0.get() = None };
        SET_STATE_FN.store(0, Ordering::SeqCst);
        GAME_TID.store(0, Ordering::SeqCst);
        set_render_only_exit(0);
    }

    /// 画面 6（Match）・カーソル `cursor` の偽のゲームを `fake_mem` に作る（場面 1・次 0、MainMenu の欄）。
    fn fake_match_screen(cursor: u32) {
        use crate::th09_addr::{
            MAIN_MENU_PTR, MENU_CURSOR, MENU_SCREEN_ID, MENU_SUBSTATE, MENU_TRANSITIONING,
            SUP_SCENE_CUR, SUP_SCENE_REQ, SUPERVISOR,
        };
        const MENU: u32 = 0x2000_0000;
        fake_mem::enable();
        fake_mem::set(SUPERVISOR + SUP_SCENE_CUR, 1);
        fake_mem::set(SUPERVISOR + SUP_SCENE_REQ, 0);
        fake_mem::set(MAIN_MENU_PTR, MENU);
        fake_mem::set(MENU + MENU_SCREEN_ID, 6);
        fake_mem::set(MENU + MENU_SUBSTATE, 1);
        fake_mem::set(MENU + MENU_TRANSITIONING, 0);
        fake_mem::set(MENU + MENU_CURSOR, cursor);
    }

    fn install_test_ui() -> &'static crate::net_ui::Board {
        let board: &'static crate::net_ui::Board = Box::leak(Box::new(crate::net_ui::Board::new()));
        let link = Arc::new(UiLink::new());
        let ini = Shape {
            mode: higanr_core::wire::Mode::Rollback,
            delay: 1,
        };
        // SAFETY: 検査は `REAL_HOOK_TESTS` の下で 1 本ずつ。`UI_ON` が 0 の間に置く（ゲームのスレッドは触らない）。
        unsafe { *UI.0.get() = Some(NetUi::new(ini, link, board)) };
        UI_ON.store(1, Ordering::Release);
        board
    }

    /// 入口を「メニューを置いた実行」の形にする: 偽の feed・ローカル入力・poll（席ごとの入力ワード `words`。席 0・1・2）・描画だけのフレームの
    /// 出口を置き、`SET_STATE_FN` を置いて、分岐を開く。
    fn arm_menu_run(words: [u32; 3]) {
        arm_io(fake_addrs(), feed_fn_addr(), local_fn_addr()).expect("arm_io");
        set_render_only_exit(crate::stub::testkit::fake_exit_addr());
        for (w, v) in POLL_WORDS.iter().zip(words) {
            w.store(v, Ordering::SeqCst);
        }
        arm_poll(fake_poll as *const () as usize as u32);
        SET_STATE_FN.store(0x1234, Ordering::SeqCst);
        let _ = install_test_ui();
        start().expect("start");
    }

    fn disarm_menu_run() {
        stop();
        // SAFETY: 分岐は閉じている。検査は REAL_HOOK_TESTS の下で 1 本ずつ。
        unsafe {
            *NET.0.get() = None;
            *UI.0.get() = None;
        }
        UI_ON.store(0, Ordering::SeqCst);
        NET_ARMED.store(0, Ordering::SeqCst);
        OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = None;
        SET_STATE_FN.store(0, Ordering::SeqCst);
        GAME_TID.store(0, Ordering::SeqCst);
        set_render_only_exit(0);
        arm_poll(0);
        fake_mem::clear();
        for w in POLL_WORDS.iter() {
            w.store(POLL_VALUE, Ordering::SeqCst);
        }
        FEED_FN.store(0, Ordering::SeqCst);
        LOCAL_FN.store(0, Ordering::SeqCst);
    }

    fn pending_box() -> Pending {
        let peer = std::net::SocketAddr::from(([127, 0, 0, 1], 9));
        let (mut net, _th) =
            net_udp::open(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), &[peer]).unwrap();
        let cfg = higanr_core::link::Config::single(
            higanr_core::handshake::Config {
                session_id: 1,
                role_nonce: 1,
                latency: 2,
                name: higanr_core::handshake::Name::new("t"),
                host_seat: higanr_core::wire::Seat::P1,
            },
            higanr_core::auth::Secret([1; 16]),
            peer,
            higanr_core::seal::KeySeed([0x4B; 32]),
        );
        let np = NetPlay::new(cfg, netplay::Config::default(), &mut net);
        Pending::Play(Box::new(NetSlot { np, net }))
    }

    fn hook_tick() -> (Vec<(u32, u32)>, u32) {
        FEED_N.store(0, Ordering::SeqCst);
        let hits0 = TARGET_HITS.load(Ordering::SeqCst);
        call_with_regs(fake_on_tick_addr());
        (feed_log(), TARGET_HITS.load(Ordering::SeqCst) - hits0)
    }

    #[test]
    fn the_tick_that_places_the_net_box_swallows_a_held_decide_and_screen_six_feeds_all_three_seats()
     {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        const Z: u32 = 0x0001;
        const DOWN: u32 = 0x0020;
        fake_match_screen(3);
        arm_menu_run([0, DOWN, DOWN]);
        POLL_N.store(0, Ordering::SeqCst);
        let (feeds, hits) = hook_tick();
        assert_eq!(
            feeds,
            vec![(0, 0), (1, DOWN), (2, DOWN)],
            "3 つの席の入力ワード"
        );
        assert_eq!(hits, 0, "画面 6 でゲーム自身の poll へ任せた");
        assert_eq!(
            POLL_N.load(Ordering::SeqCst),
            3,
            "poll は席 0・1・2 を 1 回ずつ"
        );
        disarm_menu_run();

        prepare_entry(3);
        fake_match_screen(4);
        arm_menu_run([0, Z, Z]);
        let (feeds, hits) = hook_tick();
        assert_eq!(
            feeds,
            vec![(0, 0), (1, 0), (2, 0)],
            "2P のパッドの決定が漏れた"
        );
        assert_eq!(hits, 0);
        assert!(game_ui().is_some_and(|u| u.menu_active()));
        disarm_menu_run();

        prepare_entry(3);
        fake_match_screen(4);
        arm_menu_run([Z, 0, Z]);
        OPEN_STATE.store(OPEN_BUSY, Ordering::SeqCst);
        *PENDING.lock().unwrap() = Some(Ok(pending_box()));
        assert_eq!(NET_ON.load(Ordering::SeqCst), 0);
        let (feeds, hits) = hook_tick();
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            1,
            "ネットワークのセッションを置いていない"
        );
        assert_eq!(
            feeds,
            vec![(0, 0), (1, 0), (2, 0)],
            "置いた tick に決定を feed した（漏れる）"
        );
        assert_eq!(hits, 0, "置いた tick がゲーム自身の poll へ任せた");
        assert!(
            game_ui().is_some_and(|u| u.menu_active()),
            "メニューが開いていない"
        );
        let (feeds, hits) = hook_tick();
        assert_eq!(feeds, vec![(0, 0), (1, 0), (2, 0)]);
        assert_eq!(hits, 0);
        disarm_menu_run();
    }

    #[test]
    fn a_pasted_code_connects_through_the_real_hook_while_the_menu_holds_screen_six() {
        use higanr_core::wire::Seat;
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        fake_match_screen(4);
        arm_menu_run([0, 0, 0]);
        let local = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
        let (mut net_a, th_a) = net_udp::open_for_code(local).unwrap();
        let (mut net_b, th_b) = net_udp::open_for_code(local).unwrap();
        let (addr_a, addr_b) = (net_a.local_addr().unwrap(), net_b.local_addr().unwrap());
        let cfg = netplay::Config {
            check: netplay::CheckSource::InputDigest,
            require_net_selection: true,
            ..netplay::Config::default()
        };
        let slot_a = Arc::new(netplay::LinkSlot::new());
        let slot_b = Arc::new(netplay::LinkSlot::new());
        let np_a = NetPlay::with_code_slot(Arc::clone(&slot_a), cfg, &mut net_a);
        let mut np_b = NetPlay::with_code_slot(Arc::clone(&slot_b), cfg, &mut net_b);
        assert!(install_slot(Box::new(NetSlot {
            np: np_a,
            net: net_a
        })));
        let hs = |nonce| higanr_core::handshake::Config {
            session_id: 9,
            role_nonce: nonce,
            latency: 2,
            name: higanr_core::handshake::Name::new("t"),
            host_seat: Seat::P1,
        };
        let secret = higanr_core::auth::Secret([5; 16]);
        POLL_WORDS[2].store(0x0001, Ordering::SeqCst);
        POLL_WORDS[0].store(0x0001, Ordering::SeqCst);
        let (feeds, hits) = hook_tick();
        assert_eq!(feeds, vec![(0, 0), (1, 0), (2, 0)]);
        assert_eq!(hits, 0);
        assert!(game_ui().is_some_and(|u| u.menu_active()));
        POLL_WORDS[0].store(0, Ordering::SeqCst);
        POLL_WORDS[2].store(0, Ordering::SeqCst);
        slot_a.offer(higanr_core::link::Config::single(
            hs(0xF000),
            secret,
            addr_b,
            higanr_core::seal::KeySeed([0x4B; 32]),
        ));
        slot_b.offer(higanr_core::link::Config::single(
            hs(1),
            secret,
            addr_a,
            higanr_core::seal::KeySeed([0x4B; 32]),
        ));
        let start = std::time::Instant::now();
        let mut ticks = 0u32;
        while !slot_a.is_connected() && start.elapsed() < std::time::Duration::from_secs(20) {
            let (feeds, hits) = hook_tick();
            assert_eq!(
                feeds,
                vec![(0, 0), (1, 0), (2, 0)],
                "tick {ticks}: メニューが握る tick の feed"
            );
            assert_eq!(hits, 0, "tick {ticks}: 握る tick が poll_all へ任せた");
            np_b.tend(&mut net_b);
            std::thread::sleep(std::time::Duration::from_millis(2));
            ticks += 1;
        }
        assert!(
            slot_a.is_connected(),
            "貼った後、画面 6 のメニューを開いたままでつながらない（{ticks} tick）"
        );
        for _ in 0..6 {
            let (_, hits) = hook_tick();
            assert_eq!(hits, 0);
            np_b.tend(&mut net_b);
        }
        assert!(
            game_ui().is_some_and(|u| !u.menu_active()),
            "つながってもメニューが閉じない"
        );
        disarm_menu_run();
        let _ = (th_a.stop(), th_b.stop());
    }

    #[test]
    fn poll_words_calls_the_three_seats_in_order_and_restores_each_input_manager() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        arm_io(fake_addrs(), feed_fn_addr(), local_fn_addr()).expect("arm_io");
        let init = |i: usize, j: usize| 0x11u8.wrapping_mul(i as u8 + 1).wrapping_add(j as u8);
        for (i, im) in FAKE_IMS.iter().enumerate() {
            for (j, a) in im.iter().enumerate() {
                a.store(init(i, j), Ordering::SeqCst);
            }
        }
        for (w, v) in POLL_WORDS
            .iter()
            .zip([0xDEAD_0011u32, 0xDEAD_0022, 0xDEAD_0044])
        {
            w.store(v, Ordering::SeqCst);
        }
        assert_eq!(real_io().unwrap().poll_words(), None);
        arm_poll(fake_poll as *const () as usize as u32);
        POLL_N.store(0, Ordering::SeqCst);
        let got = real_io().unwrap().poll_words();
        assert_eq!(got, Some([0x11, 0x22, 0x44]));
        assert_eq!(POLL_N.load(Ordering::SeqCst), 3, "席を 1 回ずつ");
        assert_eq!(
            POLL_SEAT.load(Ordering::SeqCst),
            2,
            "最後は席 2（poll_all と同じ順）"
        );
        for (i, im) in FAKE_IMS.iter().enumerate() {
            for (j, a) in im.iter().enumerate() {
                assert_eq!(
                    a.load(Ordering::SeqCst),
                    init(i, j),
                    "IM[{i}] +{j:#x} を戻していない"
                );
            }
        }
        arm_poll(0);
        for w in POLL_WORDS.iter() {
            w.store(POLL_VALUE, Ordering::SeqCst);
        }
        FEED_FN.store(0, Ordering::SeqCst);
        LOCAL_FN.store(0, Ordering::SeqCst);
    }
    #[test]
    fn changing_the_menu_language_sets_it_first_and_then_asks_for_the_write_once() {
        crate::ui_lang::set(Lang::Ja);
        let mut asked = Vec::new();
        apply_ui_lang(Lang::En, || {
            asked.push(crate::ui_lang::current());
        });
        assert_eq!(crate::ui_lang::current(), Lang::En);
        assert_eq!(asked, vec![Lang::En], "頼む時点で言語が置かれていない");
        apply_ui_lang(Lang::Ja, || asked.push(crate::ui_lang::current()));
        assert_eq!(crate::ui_lang::current(), Lang::Ja);
        assert_eq!(asked, vec![Lang::En, Lang::Ja]);
    }

    #[test]
    fn menu_net_follows_placed_and_asked() {
        assert_eq!(menu_net_of(false, false), MenuNet::NotMenu);
        assert_eq!(menu_net_of(false, true), MenuNet::NotMenu);
        assert_eq!(menu_net_of(true, false), MenuNet::NotAsked);
        assert_eq!(menu_net_of(true, true), MenuNet::Asked);
    }

    #[test]
    fn the_game_menu_opens_no_net_until_asked_and_the_game_thread_installs_the_box() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        arm_io(fake_addrs(), feed_fn_addr(), local_fn_addr()).expect("arm_io");
        set_render_only_exit(crate::stub::testkit::fake_exit_addr());
        let here = &FAKE_DATA as *const _ as usize as u32;
        let dir = temp_dir("game_ui");
        let s = crate::net_config::from_text(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\ncode_dir = x\n",
        )
        .unwrap()
        .unwrap();
        let ini_path = Some(dir.join("HiganRTool.ini"));
        let arm = |set_state_fn: u32, lang: Lang, ranges: &[(u32, u32)]| {
            arm_game_ui_with(
                &s,
                set_state_fn,
                Some(dir.clone()),
                ini_path.clone(),
                lang,
                ranges,
            )
        };
        assert!(arm(0x1234, Lang::En, &[(0x10, 4)]).is_err());
        assert_eq!(UI_ON.load(Ordering::SeqCst), 0);
        assert!(LAZY.get().is_none());
        assert!(arm(0, Lang::En, &[(here, 16)]).is_err());
        let desc = arm(0x1234, Lang::En, &[(here, 16)]).expect("置けない");
        assert!(desc.contains("開かない"), "{desc}");
        assert_eq!(UI_ON.load(Ordering::SeqCst), 1);
        assert_eq!(
            game_ui().expect("メニュー").lang(),
            Lang::En,
            "渡した言語でメニューが始まらない"
        );
        assert_eq!(LAZY.get().unwrap().ini_path, ini_path);
        assert!(!net_armed() && net_pending());
        assert!(
            RECV.lock().unwrap().is_none()
                && CODE_WATCH.lock().unwrap().is_none()
                && OPEN_INFO.lock().unwrap().is_none()
        );
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_IDLE);
        assert_eq!(crate::net_ui::BOARD.show(), 0, "何も描かない");
        assert!(arm(0x1234, Lang::En, &[(here, 16)]).is_err());
        start_net().expect("メニューだけの実行の start_net");
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            0,
            "ネットワークのセッションが無いので NET_ON は立てない"
        );
        start().expect("メニューだけでも分岐を開ける");
        assert_eq!(SCRIPT_ON.load(Ordering::SeqCst), 1);
        OPEN_STATE.store(OPEN_BUSY, Ordering::SeqCst);
        *PENDING.lock().unwrap() = Some(Err(()));
        poll_pending();
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_IDLE);
        let note = LAZY
            .get()
            .unwrap()
            .link
            .note()
            .expect("メニューへ知らせていない");
        assert_eq!(note.event, Event::Internal);
        let tries = OPEN_TRIES.load(Ordering::SeqCst);
        assert!(open_job(|| Err("検査: 開けない".to_string())).is_err());
        assert_eq!(OPEN_TRIES.load(Ordering::SeqCst), tries + 1);
        assert_eq!(menu_net(), MenuNet::Asked);
        assert!(open_job(|| panic!("検査: 開く仕事のパニック")).is_err());
        {
            let notes = GAME_NET_NOTES.lock().unwrap();
            assert!(notes.iter().any(|l| l.contains("検査: 開けない")));
            assert!(notes.iter().any(|l| l.contains("パニック")));
        }
        let peer = std::net::SocketAddr::from(([127, 0, 0, 1], 9));
        let (mut net, _th) =
            net_udp::open(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), &[peer]).unwrap();
        let cfg = higanr_core::link::Config::single(
            higanr_core::handshake::Config {
                session_id: 1,
                role_nonce: 1,
                latency: 2,
                name: higanr_core::handshake::Name::new("t"),
                host_seat: higanr_core::wire::Seat::P1,
            },
            higanr_core::auth::Secret([1; 16]),
            peer,
            higanr_core::seal::KeySeed([0x4B; 32]),
        );
        let np = NetPlay::new(cfg, netplay::Config::default(), &mut net);
        *PENDING.lock().unwrap() = Some(Ok(Pending::Play(Box::new(NetSlot { np, net }))));
        OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
        poll_pending();
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            0,
            "開いている最中でなければ取らない"
        );
        assert!(PENDING.lock().unwrap().is_some());
        OPEN_STATE.store(OPEN_BUSY, Ordering::SeqCst);
        poll_pending();
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            1,
            "ネットワークのセッションを置いて動かした"
        );
        assert!(net_armed() && !net_pending());
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_DONE);
        assert!(PENDING.lock().unwrap().is_none());
        UI_ON.store(0, Ordering::SeqCst);
        for _ in 0..2 {
            call_with_regs(fake_on_tick_addr());
        }
        assert_eq!(
            NET_STATUS.snapshot().ticks,
            2,
            "ネットワークの経路に移っていない（置いたネットワークのセッションの tick が 2 回進んでいない）"
        );
        stop();
        // SAFETY: 分岐は閉じている。検査は REAL_HOOK_TESTS の下で 1 本ずつ。
        unsafe {
            *NET.0.get() = None;
            *UI.0.get() = None;
        }
        NET_ARMED.store(0, Ordering::SeqCst);
        OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
        SET_STATE_FN.store(0, Ordering::SeqCst);
        GAME_TID.store(0, Ordering::SeqCst);
        set_render_only_exit(0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ended_menu_connection_is_handed_off_outside_the_match_and_reopened() {
        use crate::th09_addr::{SUP_SCENE_CUR, SUPERVISOR};
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        fake_match_screen(2);
        arm_menu_run([0, 0, 0]);
        let link = Arc::clone(game_ui().expect("メニュー").link());
        let s = crate::net_config::from_text(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\n",
        )
        .unwrap()
        .unwrap();
        let net_code_game::Opened {
            mut np,
            net,
            watch,
            recv,
            local,
            ..
        } = net_code_game::open_set(
            &s,
            play_config(&s, false),
            7,
            net_code::Timing::default(),
            || "（検査: 知らせない）".to_string(),
            net_code::ModeCtx::none(),
            GameUi {
                link: Arc::clone(&link),
                clip: Box::new(crate::clipboard::fake::FakeClipboard::default()),
                room: None,
                room_last_dir: None,
                spent: Vec::new(),
            },
        )
        .expect("ネットワークの組を開けない");
        let port = local.expect("受信ソケット").port();
        np.close(higanr_core::wire::ByeReason::Cancelled);
        *CODE_WATCH.lock().unwrap() = Some(watch);
        *RECV.lock().unwrap() = Some(recv);
        OPEN_STATE.store(OPEN_BUSY, Ordering::SeqCst);
        *PENDING.lock().unwrap() = Some(Ok(Pending::Play(Box::new(NetSlot { np, net }))));
        /// 途中で落ちても、ほかの本物の入口の検査へ静的な値（ネットワークのセッション・メニュー・郵便受け・ワーカーの印）を残さない。
        struct Restore {
            relink: u32,
            grace: u32,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                disarm_menu_run();
                *CODE_WATCH.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *OPEN_INFO.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *CLOSING.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *RECV.lock().unwrap_or_else(|e| e.into_inner()) = None;
                RELINK_STARTED.store(self.relink, Ordering::SeqCst);
                RELINK_GRACE.store(self.grace, Ordering::SeqCst);
            }
        }
        let _restore = Restore {
            relink: RELINK_STARTED.swap(1, Ordering::SeqCst),
            grace: RELINK_GRACE.swap(0, Ordering::SeqCst),
        };
        let closed0 = LINKS_CLOSED.load(Ordering::SeqCst);
        fake_mem::set(SUPERVISOR + SUP_SCENE_CUR, 2);
        for t in 0..5 {
            hook_tick();
            assert_eq!(
                NET_ON.load(Ordering::SeqCst),
                1,
                "tick {t}: 試合の中で手放した"
            );
        }
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_DONE);
        fake_mem::set(SUPERVISOR + SUP_SCENE_CUR, 1);
        RELINK_STARTED.store(0, Ordering::SeqCst);
        hook_tick();
        assert_eq!(NET_ON.load(Ordering::SeqCst), 1);
        RELINK_STARTED.store(1, Ordering::SeqCst);
        let c = higanr_core::wire::Check {
            seg: 0,
            frame: 5,
            rng_state: 1,
            rng_count: 2,
            life_1p: 3,
            life_2p: 4,
        };
        NET_STATUS.fake_desync(&c, &c);
        RB_ABORTS.store(3, Ordering::SeqCst);
        FEED_N.store(0, Ordering::SeqCst);
        let hits0 = TARGET_HITS.load(Ordering::SeqCst);
        let a0 = crate::test_alloc::count();
        call_with_regs(fake_on_tick_addr());
        call_with_regs(fake_on_tick_addr());
        let a1 = crate::test_alloc::count();
        assert_eq!(
            a1, a0,
            "手放す tick と次の tick のゲームのスレッドで確保した"
        );
        assert_eq!(TARGET_HITS.load(Ordering::SeqCst), hits0);
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            0,
            "試合の外へ戻っても手放さない"
        );
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_CLOSING);
        assert!(CLOSING.lock().unwrap().is_some(), "郵便受けに置いていない");
        assert_eq!(game_ui().unwrap().stats().teardowns, 1);
        let (feeds, hits) = hook_tick();
        assert_eq!(feeds, vec![(0, 0), (1, 0), (2, 0)]);
        assert_eq!(hits, 0);
        request_open();
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_REOPEN);
        let mut reopened = false;
        relink_tick_with(|| {
            reopened = true;
            Some(Ok(pending_box()))
        });
        assert!(reopened, "片づけている間に選んだ「作る」で開き直さない");
        assert_eq!(LINKS_CLOSED.load(Ordering::SeqCst), closed0 + 1);
        assert!(CLOSING.lock().unwrap().is_none());
        assert!(RECV.lock().unwrap().is_none(), "受信スレッドを止めていない");
        assert!(
            CODE_WATCH
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(net_code::Watch::finished),
            "見張りを止めていない"
        );
        std::net::UdpSocket::bind(("127.0.0.1", port)).expect("受信ソケットが閉じていない");
        let past = PAST_LINKS
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default();
        assert!(
            past.contains(&format!("{} 回目のつながり（片づけた", closed0 + 1))
                && past.contains("ネットワーク: 段階 終えた"),
            "{past}"
        );
        assert!(
            past.contains("CHECK のズレ"),
            "前のつながりのネットワークの行: {past}"
        );
        let (snap, other) = net_status();
        assert!(
            other.contains("回目のつながり（片づけた"),
            "本ログのネットワークの外の文に出ない"
        );
        assert!(
            snap.link_ended() && snap.relink && !snap.ended(),
            "{}",
            snap.text()
        );
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_BUSY);
        hook_tick();
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            1,
            "開き直したネットワークのセッションを置いていない"
        );
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_DONE);
        assert_eq!(
            NET_STATUS.snapshot().desync,
            None,
            "CHECK のズレの欄を持ち越した"
        );
        assert_eq!(
            RB_ABORTS.load(Ordering::SeqCst),
            0,
            "試合をやめた数を持ち越した"
        );
        // SAFETY: ゲームのスレッドは居ない（検査のスレッドが入口を呼ぶ）。REAL_HOOK_TESTS の下で 1 本ずつ。
        if let Some(Slot::Play(s)) = unsafe { (*NET.0.get()).as_mut() } {
            s.np.close(higanr_core::wire::ByeReason::Cancelled);
        }
        UI_ON.store(0, Ordering::SeqCst);
        for _ in 0..3 {
            hook_tick();
        }
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            1,
            "メニューの無い走りで手放した"
        );
        assert!(!net_status().0.relink);
    }

    fn watch_box() -> (Box<WatchNetSlot>, RecvThread) {
        let (net, th) =
            net_udp::open_for_code(std::net::SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let wp = WatchPlay::new(
            Arc::new(WatchSlot::new()),
            WatchConfig {
                wait_budget_us: 16_000,
                margin_frames: WATCH_MARGIN_FRAMES,
                require_net_selection: true,
            },
        );
        (
            Box::new(WatchNetSlot {
                wp,
                net,
                shots: Default::default(),
            }),
            th,
        )
    }

    fn post_watch_box() -> RecvThread {
        let (w, th) = watch_box();
        OPEN_STATE.store(OPEN_BUSY, Ordering::SeqCst);
        *PENDING.lock().unwrap() = Some(Ok(Pending::Watch(w)));
        th
    }

    fn with_watch_play<R>(f: impl FnOnce(&mut WatchPlay) -> R) -> R {
        // SAFETY: ゲームのスレッドは居ない（検査のスレッドが入口を呼ぶ）。REAL_HOOK_TESTS の下で 1 本ずつ。
        match unsafe { (*NET.0.get()).as_mut() } {
            Some(Slot::Watch(w)) => f(&mut w.wp),
            _ => panic!("観戦者のセッションが置かれていない"),
        }
    }

    /// ワーカーの印と待ちを検査用に替え、落ちるときも元に戻す（ほかの本物の入口の検査へ静的な値を残さない）。
    struct WatchRestore {
        relink: u32,
    }

    impl WatchRestore {
        fn new() -> WatchRestore {
            WatchRestore {
                relink: RELINK_STARTED.swap(1, Ordering::SeqCst),
            }
        }
    }

    impl Drop for WatchRestore {
        fn drop(&mut self) {
            disarm_menu_run();
            *CLOSING.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *RECV.lock().unwrap_or_else(|e| e.into_inner()) = None;
            PAST_WATCHES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            let _ = take_watch_ends();
            SPECTATE_AFTER.store(0, Ordering::SeqCst);
            FORCE_SPECTATE_OPEN.store(0, Ordering::SeqCst);
            RELINK_STARTED.store(self.relink, Ordering::SeqCst);
            crate::spectate_join::set_run_for_test(0);
        }
    }

    #[test]
    fn a_watcher_box_placed_later_keeps_the_entry_check_and_does_not_allocate() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        const Z: u32 = 0x0001;
        fake_match_screen(4);
        arm_menu_run([Z, 0, Z]);
        let _restore = WatchRestore::new();
        let th = post_watch_box();
        assert_eq!(NET_ON.load(Ordering::SeqCst), 0);
        let (feeds, hits) = hook_tick();
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            1,
            "観戦者のセッションを置いていない"
        );
        assert!(
            matches!(unsafe { (*NET.0.get()).as_ref() }, Some(Slot::Watch(_))),
            "置いたのが観戦者のセッションでない"
        );
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_DONE);
        assert_eq!(
            feeds,
            vec![(0, 0), (1, 0), (2, 0)],
            "置いた tick に決定を feed した（漏れる）"
        );
        assert_eq!(hits, 0, "置いた tick がゲーム自身の poll へ任せた");
        assert!(
            game_ui().is_some_and(|u| u.menu_active()),
            "観戦者のセッションでメニューが開いていない"
        );
        let (feeds, hits) = hook_tick();
        assert_eq!(feeds, vec![(0, 0), (1, 0), (2, 0)]);
        assert_eq!(hits, 0);
        let a0 = crate::test_alloc::count();
        for _ in 0..5 {
            call_with_regs(fake_on_tick_addr());
        }
        assert_eq!(
            crate::test_alloc::count(),
            a0,
            "観戦者のセッションの tick のゲームのスレッドで確保した"
        );
        let _ = th.stop();
    }

    #[test]
    fn a_finished_watcher_box_is_released_only_outside_the_match_once_the_room_thread_is_done() {
        use crate::th09_addr::{SUP_SCENE_CUR, SUPERVISOR};
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _j = crate::spectate_join::tests::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        fake_match_screen(2);
        arm_menu_run([0, 0, 0]);
        let _restore = WatchRestore::new();
        let th = post_watch_box();
        hook_tick();
        assert_eq!(NET_ON.load(Ordering::SeqCst), 1);
        let watches0 = WATCHES_CLOSED.load(Ordering::SeqCst);
        crate::spectate_join::set_run_for_test(2);
        for _ in 0..3 {
            hook_tick();
        }
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            1,
            "終わっていないセッションを手放した"
        );
        with_watch_play(|wp| wp.set_phase_for_test(netplay::Phase::Ended));
        crate::spectate_join::set_run_for_test(1);
        for _ in 0..3 {
            hook_tick();
        }
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            1,
            "ルームのスレッドが終わりの状態を書き終える前に手放した"
        );
        crate::spectate_join::set_run_for_test(2);
        fake_mem::set(SUPERVISOR + SUP_SCENE_CUR, 2);
        for _ in 0..3 {
            hook_tick();
        }
        assert_eq!(NET_ON.load(Ordering::SeqCst), 1, "試合の中で手放した");
        fake_mem::set(SUPERVISOR + SUP_SCENE_CUR, 1);
        RELINK_STARTED.store(0, Ordering::SeqCst);
        hook_tick();
        assert_eq!(NET_ON.load(Ordering::SeqCst), 1);
        RELINK_STARTED.store(1, Ordering::SeqCst);
        game_ui()
            .unwrap()
            .link()
            .send(higanr_core::net_menu::Cmd::SpectateCancel);
        let a0 = crate::test_alloc::count();
        call_with_regs(fake_on_tick_addr());
        call_with_regs(fake_on_tick_addr());
        assert_eq!(
            crate::test_alloc::count(),
            a0,
            "手放す tick と次の tick のゲームのスレッドで確保した"
        );
        assert_eq!(NET_ON.load(Ordering::SeqCst), 0, "手放していない");
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_CLOSING);
        assert!(
            matches!(
                CLOSING.lock().unwrap().as_ref().map(|c| &c.slot),
                Some(Released::Watch(_))
            ),
            "郵便受けに観戦者のセッションを置いていない"
        );
        assert_eq!(game_ui().unwrap().stats().teardowns, 1);
        let (feeds, hits) = hook_tick();
        assert_eq!(feeds, vec![(0, 0), (1, 0), (2, 0)]);
        assert_eq!(hits, 0);
        let link = Arc::clone(game_ui().unwrap().link());
        link.publish(&Note {
            seq: 77,
            event: Event::SpectateEnded,
            state: CodeState::Idle,
            ..Note::default()
        });
        let mut reopened = false;
        relink_tick_with(|| {
            reopened = true;
            None
        });
        assert!(!reopened, "何も選ばれていないのに開き直した");
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_IDLE);
        assert!(CLOSING.lock().unwrap().is_none());
        assert_eq!(
            link.take_order(),
            None,
            "指示のリングバッファに残った指示を捨てていない（溜まると新しい指示を捨てる）"
        );
        assert_eq!(
            link.note().map(|n| (n.seq, n.event)),
            Some((77, Event::SpectateEnded)),
            "観戦の終わりの理由を消した（対戦者のセッションの片づけ `reset_for_next_watcher` と取り違えた）"
        );
        assert_eq!(WATCHES_CLOSED.load(Ordering::SeqCst), watches0 + 1);
        let past = PAST_WATCHES.lock().unwrap().concat();
        assert!(
            past.contains(&format!("{} 回目の観戦（片づけた", watches0 + 1)),
            "{past}"
        );
        assert!(
            !crate::spectate_join::finished(),
            "片づけた後も終わった印が残る"
        );
        let (_, other) = net_status();
        assert!(
            other.contains("回目の観戦（片づけた"),
            "本ログに出ない: {other}"
        );
        let _ = th.stop();
    }

    #[test]
    fn a_closed_watcher_box_hands_its_room_counts_to_the_log_once() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _j = crate::spectate_join::tests::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::spectate_join::tests::reset_globals();
        PAST_WATCHES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let _ = take_watch_ends();
        assert_eq!(watch_live_text(), None);
        let _shared = crate::spectate_join::tests::plant_active_for_test();
        let live = watch_live_text().expect("セッションがあるのに途中経過の材料が無い");
        assert!(
            live.contains("観戦者のルーム（") && live.contains("入った 1"),
            "{live}"
        );
        assert!(take_watch_ends().is_empty(), "片づける前に節がある");
        let n0 = WATCHES_CLOSED.load(Ordering::SeqCst);
        let (w, th) = watch_box();
        let link = Arc::new(crate::net_ui::UiLink::new());
        let mark = link.order_mark();
        close_watch(*w, &link, mark);
        let _ = th.stop();
        assert_eq!(WATCHES_CLOSED.load(Ordering::SeqCst), n0 + 1);
        assert_eq!(watch_live_text(), None, "片づけた後も途中経過の材料が残る");
        let ends = take_watch_ends();
        assert_eq!(ends.len(), 1, "{ends:?}");
        assert_eq!(ends[0].0, n0 + 1, "節の番号が「n 回目の観戦」と違う");
        assert!(
            ends[0].1.contains("入った 1") && ends[0].1.contains("受信 "),
            "{ends:?}"
        );
        assert!(take_watch_ends().is_empty(), "同じ節を 2 回取り出せる");
        let (_, other) = net_status();
        assert_eq!(
            other
                .matches("観戦者のルーム（観戦用のルームへ入り")
                .count(),
            1,
            "{other}"
        );
        assert!(other.contains(&format!("{} 回目の観戦（片づけた", n0 + 1)));
        crate::spectate_join::tests::reset_globals();
        PAST_WATCHES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    #[test]
    fn request_spectate_follows_the_open_state() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _j = crate::spectate_join::tests::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        fake_match_screen(2);
        arm_menu_run([0, 0, 0]);
        let _restore = WatchRestore::new();
        if LAZY.get().is_none() {
            OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
            SPECTATE_AFTER.store(1, Ordering::SeqCst);
            request_spectate();
            assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_IDLE);
            assert_eq!(
                SPECTATE_AFTER.load(Ordering::SeqCst),
                0,
                "観戦の予定が残った"
            );
            SPECTATE_AFTER.store(1, Ordering::SeqCst);
            request_open();
            assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_IDLE);
            assert_eq!(
                SPECTATE_AFTER.load(Ordering::SeqCst),
                0,
                "「作る」の頼みが観戦の予定を残した"
            );
        }
        OPEN_STATE.store(OPEN_CLOSING, Ordering::SeqCst);
        request_spectate();
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_REOPEN);
        assert_eq!(SPECTATE_AFTER.load(Ordering::SeqCst), 1);
        request_spectate();
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_REOPEN);
        OPEN_STATE.store(OPEN_BUSY, Ordering::SeqCst);
        request_spectate();
        assert_eq!(
            OPEN_STATE.load(Ordering::SeqCst),
            OPEN_BUSY,
            "開いている最中に 2 本目を起こした"
        );
        OPEN_STATE.store(OPEN_CLOSING, Ordering::SeqCst);
        SPECTATE_AFTER.store(1, Ordering::SeqCst);
        request_open();
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_REOPEN);
        assert_eq!(
            SPECTATE_AFTER.load(Ordering::SeqCst),
            0,
            "観戦の予定が残った"
        );
        *CLOSING.lock().unwrap() = None;
        let (w, th0) = watch_box();
        *CLOSING.lock().unwrap() = Some(Closing {
            slot: Released::Watch(w),
            link: Arc::new(UiLink::new()),
            mark: 0,
        });
        OPEN_STATE.store(OPEN_CLOSING, Ordering::SeqCst);
        SPECTATE_AFTER.store(1, Ordering::SeqCst);
        let mut opened = false;
        relink_tick_with(|| {
            opened = true;
            None
        });
        assert!(opened, "観戦の予定があるのに、手放した後に開かない");
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_IDLE);
        let _ = th0.stop();
        OPEN_STATE.store(OPEN_BUSY, Ordering::SeqCst);
        *PENDING.lock().unwrap() = Some(Ok(pending_box()));
        hook_tick();
        assert_eq!(NET_ON.load(Ordering::SeqCst), 1);
        request_spectate();
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            0,
            "待っているだけの対戦者のセッションを手放していない"
        );
        assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_CLOSING);
        assert_eq!(SPECTATE_AFTER.load(Ordering::SeqCst), 1);
        assert!(matches!(
            CLOSING.lock().unwrap().as_ref().map(|c| &c.slot),
            Some(Released::Play(_))
        ));
        *CLOSING.lock().unwrap() = None;
        SPECTATE_AFTER.store(0, Ordering::SeqCst);
        OPEN_STATE.store(OPEN_BUSY, Ordering::SeqCst);
        let Pending::Play(mut p) = pending_box() else {
            panic!("対戦者のセッションでない")
        };
        p.np.close(higanr_core::wire::ByeReason::Cancelled);
        *PENDING.lock().unwrap() = Some(Ok(Pending::Play(p)));
        hook_tick();
        let refused1 = SPECTATE_REFUSED.load(Ordering::Relaxed);
        request_spectate();
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            1,
            "終わりかけの対戦者のセッションを手放した"
        );
        assert_eq!(SPECTATE_REFUSED.load(Ordering::Relaxed), refused1 + 1);
        assert_eq!(SPECTATE_AFTER.load(Ordering::SeqCst), 0);
        stop();
        // SAFETY: 分岐は閉じている。検査は REAL_HOOK_TESTS の下で 1 本ずつ。
        unsafe { *NET.0.get() = None };
        NET_ON.store(0, Ordering::SeqCst);
        SCRIPT_ON.store(1, Ordering::SeqCst);
        let th = post_watch_box();
        hook_tick();
        assert!(matches!(
            unsafe { (*NET.0.get()).as_ref() },
            Some(Slot::Watch(_))
        ));
        crate::spectate_join::set_run_for_test(2);
        let refused2 = SPECTATE_REFUSED.load(Ordering::Relaxed);
        request_spectate();
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            1,
            "終わっていない観戦者のセッションを手放した"
        );
        assert_eq!(SPECTATE_REFUSED.load(Ordering::Relaxed), refused2 + 1);
        with_watch_play(|wp| wp.set_phase_for_test(netplay::Phase::Ended));
        crate::spectate_join::set_run_for_test(1);
        request_spectate();
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            1,
            "ルームのスレッドが終わる前に手放した"
        );
        crate::spectate_join::set_run_for_test(2);
        request_spectate();
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            0,
            "終わった観戦者のセッションを手放していない"
        );
        assert_eq!(SPECTATE_AFTER.load(Ordering::SeqCst), 1);
        let _ = th.stop();
    }

    #[test]
    fn the_three_entry_paths_pass_the_spectate_decision_on() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _j = crate::spectate_join::tests::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        fake_match_screen(2);
        arm_menu_run([0, 0, 0]);
        let _restore = WatchRestore::new();
        let asked = || SPECTATE_REQUESTS.load(Ordering::Relaxed);
        let n0 = asked();
        FORCE_SPECTATE_OPEN.store(1, Ordering::SeqCst);
        hook_tick();
        assert_eq!(asked(), n0 + 1, "メニューだけの経路が決定を見ない");
        if LAZY.get().is_none() {
            assert_eq!(OPEN_STATE.load(Ordering::SeqCst), OPEN_IDLE);
        }
        OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
        OPEN_STATE.store(OPEN_BUSY, Ordering::SeqCst);
        *PENDING.lock().unwrap() = Some(Ok(pending_box()));
        hook_tick();
        assert_eq!(NET_ON.load(Ordering::SeqCst), 1);
        let n1 = asked();
        FORCE_SPECTATE_OPEN.store(1, Ordering::SeqCst);
        hook_tick();
        assert_eq!(asked(), n1 + 1, "対戦者のセッションの経路が決定を見ない");
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            0,
            "待っているセッションを手放していない"
        );
        assert_eq!(SPECTATE_AFTER.load(Ordering::SeqCst), 1);
        *CLOSING.lock().unwrap() = None;
        SPECTATE_AFTER.store(0, Ordering::SeqCst);
        OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
        stop();
        // SAFETY: 分岐は閉じている。検査は REAL_HOOK_TESTS の下で 1 本ずつ。
        unsafe { *NET.0.get() = None };
        NET_ON.store(0, Ordering::SeqCst);
        SCRIPT_ON.store(1, Ordering::SeqCst);
        let th = post_watch_box();
        hook_tick();
        assert!(matches!(
            unsafe { (*NET.0.get()).as_ref() },
            Some(Slot::Watch(_))
        ));
        let n2 = asked();
        FORCE_SPECTATE_OPEN.store(1, Ordering::SeqCst);
        hook_tick();
        assert_eq!(asked(), n2 + 1, "観戦者のセッションの経路が決定を見ない");
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            1,
            "観戦中のセッションを手放した"
        );
        let _ = th.stop();
    }

    #[test]
    fn opening_the_watcher_net_builds_the_box_and_starts_the_room_thread() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _j = crate::spectate_join::tests::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::spectate_join::tests::reset_globals();
        let relink = RELINK_STARTED.swap(1, Ordering::SeqCst);
        let mk = |extra: &str| -> &'static LazyNet {
            let s = crate::net_config::from_text(&format!(
                "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\nname = viewer\ndelay = 3\n{extra}"
            ))
            .unwrap()
            .unwrap();
            Box::leak(Box::new(LazyNet {
                settings: s,
                code_dir: None,
                ini_path: None,
                link: Arc::new(UiLink::new()),
            }))
        };
        let lazy = mk("");
        lazy.link.send(higanr_core::net_menu::Cmd::Spectate(
            higanr_core::room_input::RoomBuf::from_name("synth-open").unwrap(),
        ));
        let e = open_watch_net(lazy)
            .err()
            .expect("ルームサーバーが無いのに開いた");
        assert!(e.contains("room_server"), "{e}");
        let lazy2 = mk("room_server = ws://127.0.0.1:9\n");
        assert!(open_watch_net(lazy2).is_err(), "名が無いのに開いた");
        lazy2.link.send(higanr_core::net_menu::Cmd::Spectate(
            higanr_core::room_input::RoomBuf::from_name("synth-open").unwrap(),
        ));
        let w = open_watch_net(lazy2).expect("観戦者のネットワークを開けない");
        let n = lazy2.link.note().expect("最初の状態が書かれていない");
        assert!(
            n.state.is_spectate_busy() || n.state == CodeState::Idle,
            "最初の状態 {:?}",
            n.state
        );
        assert!(n.seq >= 1);
        let shown: String = n
            .room
            .iter()
            .take_while(|&&u| u != 0)
            .map(|&u| char::from_u32(u32::from(u)).unwrap_or('?'))
            .collect();
        assert_eq!(shown, "synth-open", "ルーム名が画面用の状態に載っていない");
        assert!(w.net.local_addr().is_some());
        assert!(RECV.lock().unwrap().is_some(), "受信スレッドを置いていない");
        assert!(OPEN_INFO.lock().unwrap().is_some());
        lazy2.link.send(higanr_core::net_menu::Cmd::SpectateCancel);
        let t0 = std::time::Instant::now();
        while !crate::spectate_join::finished() {
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(5),
                "取りやめで終わらない"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let n = lazy2.link.note().unwrap();
        assert_eq!(
            (n.state, n.event),
            (CodeState::Idle, Event::SpectateCancelled)
        );
        let notes = GAME_NET_NOTES.lock().unwrap().join("\n");
        assert!(notes.contains("観戦者のネットワークを開いた"), "{notes}");
        assert!(
            notes.contains("観戦: 早送り: 走査の call を書けなかった"),
            "{notes}"
        );
        assert!(
            !crate::rollback_win::scan_hooked(),
            "検査のプロセスで走査の call を書いた"
        );
        assert!(!notes.to_lowercase().contains("synth-open"), "{notes}");
        let WatchNetSlot { wp, net, .. } = *w;
        drop((wp, net));
        crate::spectate_join::stop_and_join();
        if let Some(r) = RECV.lock().unwrap().take() {
            let _ = r.stop();
        }
        *OPEN_INFO.lock().unwrap() = None;
        RELINK_STARTED.store(relink, Ordering::SeqCst);
        crate::spectate_join::tests::reset_globals();
    }

    #[test]
    fn a_watcher_box_runs_through_the_real_hook_and_lets_the_title_run() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        arm_io(fake_addrs(), feed_fn_addr(), local_fn_addr()).expect("arm_io");
        set_render_only_exit(crate::stub::testkit::fake_exit_addr());
        let (net, _th) =
            net_udp::open_for_code(std::net::SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let wp = WatchPlay::new(
            std::sync::Arc::new(WatchSlot::new()),
            WatchConfig {
                wait_budget_us: 16_000,
                margin_frames: WATCH_MARGIN_FRAMES,
                require_net_selection: false,
            },
        );
        let here = &FAKE_DATA as *const _ as usize as u32;
        let slot = Slot::Watch(Box::new(WatchNetSlot {
            wp,
            net,
            shots: Default::default(),
        }));
        arm_slot(slot, 0x1234, &[(here, 16)]).expect("観戦者のセッションを置けない");
        start_net().expect("start_net");
        start().expect("観戦者のセッションが動いているのに分岐を開かない");
        let ticks0 = NET_STATUS.snapshot().ticks;
        let local0 = LOCAL_N.load(Ordering::SeqCst);
        for t in 0..4 {
            FEED_N.store(0, Ordering::SeqCst);
            let hits0 = TARGET_HITS.load(Ordering::SeqCst);
            call_with_regs(fake_on_tick_addr());
            assert_eq!(
                TARGET_HITS.load(Ordering::SeqCst) - hits0,
                1,
                "tick {t}: 元の飛び先"
            );
            assert!(feed_log().is_empty(), "tick {t}: feed した");
            let after = load(&AFTER);
            assert_eq!(after[3], REG_EBX, "tick {t}: ebx");
            assert_eq!(after[4], fake_this_addr(), "tick {t}: esi");
            assert_eq!(after[5], REG_EDI, "tick {t}: edi");
            assert_eq!(after[6], REG_EBP, "tick {t}: ebp");
            assert_eq!(after[7], PRE_ESP.load(Ordering::SeqCst), "tick {t}: esp");
        }
        let _ = ticks0;
        assert_eq!(NET_STATUS.snapshot().ticks, 4, "観戦者の判断を呼んでいない");
        assert_eq!(
            LOCAL_N.load(Ordering::SeqCst),
            local0,
            "コードの無い間にローカル入力を読んだ"
        );
        assert_eq!(NET_STATUS.snapshot().phase, 0, "繋がる前の段階");
        match_start_on();
        assert_eq!(NET_PANICKED.load(Ordering::SeqCst), 0);
        stop();
        // SAFETY: 分岐は閉じている。検査は REAL_HOOK_TESTS の下で 1 本ずつ。
        unsafe { *NET.0.get() = None };
        SET_STATE_FN.store(0, Ordering::SeqCst);
        GAME_TID.store(0, Ordering::SeqCst);
        set_render_only_exit(0);
    }

    #[test]
    fn the_save_view_of_a_watcher_box_carries_its_names_or_says_it_is_waiting() {
        use crate::replay_save::Watching;
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let (net, _th) =
            net_udp::open_for_code(std::net::SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let wp = WatchPlay::new(
            std::sync::Arc::new(WatchSlot::new()),
            WatchConfig {
                wait_budget_us: 16_000,
                margin_frames: WATCH_MARGIN_FRAMES,
                require_net_selection: false,
            },
        );
        let mut slot = Slot::Watch(Box::new(WatchNetSlot {
            wp,
            net,
            shots: Default::default(),
        }));
        let (np, w) = save_view(Some(&slot));
        assert!(np.is_none());
        assert_eq!(w, Watching::WaitingForNames, "名前を受けていない観戦者");
        if let Slot::Watch(s) = &mut slot {
            s.wp.set_names_for_test("いちろう", "B");
        }
        let (np, w) = save_view(Some(&slot));
        assert!(np.is_none());
        assert_eq!(
            w,
            Watching::Names([
                higanr_core::handshake::Name::new("いちろう"),
                higanr_core::handshake::Name::new("B")
            ])
        );
        assert_eq!(save_view(None).1, Watching::No);
    }

    #[test]
    fn arm_and_start_refuse_bad_setups() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        let s = || parse("scene=3 p1=Z\n").unwrap();
        assert!(arm(s(), fake_addrs(), 0, local_fn_addr()).is_err());
        let mut bad = fake_addrs();
        bad.pause = 0x10;
        assert!(
            arm(s(), bad, feed_fn_addr(), local_fn_addr())
                .unwrap_err()
                .contains("0x00000010")
        );
        arm(s(), fake_addrs(), feed_fn_addr(), local_fn_addr()).expect("arm");
        let t0 = progress().ticks;
        FEED_N.store(0, Ordering::SeqCst);
        call_with_regs(fake_on_tick_addr());
        assert_eq!(progress().ticks, t0);
        assert!(feed_log().is_empty());
        start().unwrap();
        assert!(arm(s(), fake_addrs(), feed_fn_addr(), local_fn_addr()).is_err());
        stop();
    }

    #[test]
    fn panic_in_tick_stops_script_and_shots() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        arm(
            parse("scene=3 p1=Z\n").unwrap(),
            fake_addrs(),
            feed_fn_addr(),
            local_fn_addr(),
        )
        .unwrap();
        start().unwrap();
        shot_win::set_on();
        PANIC_NEXT.store(true, Ordering::SeqCst);
        assert_eq!(script_on_tick(5), 0);
        assert_eq!(SCRIPT_ON.load(Ordering::SeqCst), 0);
        assert_eq!(shot_win::SHOT_ON.load(Ordering::SeqCst), 0);
        assert_eq!(progress().state, RunState::Panicked);
    }

    #[test]
    fn held_box_without_order_returns_one_and_counts_stray() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        arm(
            parse("scene=3 wait=100 p1=Z\n").unwrap(),
            fake_addrs(),
            feed_fn_addr(),
            local_fn_addr(),
        )
        .unwrap();
        start().unwrap();
        let (fed0, stray0) = (progress().fed, RB_STRAY.load(Ordering::SeqCst));
        RB_HELD.store(1, Ordering::SeqCst);
        let r = script_on_tick(0);
        RB_HELD.store(0, Ordering::SeqCst);
        stop();
        assert_eq!(r, 1, "0 を返した（poll_all へ飛ぶ）");
        assert_eq!(RB_STRAY.load(Ordering::SeqCst), stray0 + 1, "数えない");
        assert_eq!(progress().fed, fed0, "台本が進んだ");
        let stray = |n| RbCounts {
            stray: n,
            ..RbCounts::default()
        };
        let t = rollback_text_with(Some("S".into()), stray(2));
        assert!(t.contains("入力の入口 2 回"), "本ログに出ない: {t}");
        assert!(!rollback_text_with(Some("S".into()), stray(0)).contains("入力の入口"));
    }

    #[test]
    fn rollback_text_names_the_abort_path() {
        for a in [
            netplay::RbAbort::CoreFault,
            netplay::RbAbort::ScanBroken,
            netplay::RbAbort::PostBroken,
            netplay::RbAbort::Stray,
        ] {
            assert_eq!(abort_of_code(abort_code(a)), Some(a));
            let t = rollback_text_with(
                Some("S".into()),
                RbCounts {
                    aborts: 1,
                    first: abort_code(a),
                    first_frame: 1259,
                    ..RbCounts::default()
                },
            );
            assert!(
                t.contains(&format!(
                    "試合をやめた 1 回。最初は「{}」（確定したフレーム 1259 の後）",
                    a.text()
                )),
                "{t}"
            );
        }
        assert_eq!(abort_of_code(0), None);
        let quiet = rollback_text_with(Some("S".into()), RbCounts::default());
        assert!(
            !quiet.contains("やめた") && !quiet.contains("借りられず"),
            "{quiet}"
        );
        let t = rollback_text_with(
            Some("S".into()),
            RbCounts {
                bypass: 3,
                ..RbCounts::default()
            },
        );
        assert!(t.contains("走査を普段どおりに回した 3 回"), "{t}");
    }

    #[test]
    fn script_on_tick_does_not_allocate() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(1);
        static MENU: [AtomicU32; 0x1B240 / 4] = [const { AtomicU32::new(0) }; 0x1B240 / 4];
        FAKE_DATA[3].store(&MENU as *const _ as usize as u32, Ordering::SeqCst);
        crate::shot_win::tests::setup();
        arm(
            parse("scene=1 wait=2 p1=DOWN hold=3 inv=p1+p2 life1=3 gauge1=100\n").unwrap(),
            fake_addrs(),
            feed_fn_addr(),
            local_fn_addr(),
        )
        .unwrap();
        start().unwrap();
        let (s0, s1) = fake_player_slots();
        STATE_SLOTS[0].store(s0, Ordering::SeqCst);
        STATE_SLOTS[1].store(s1, Ordering::SeqCst);
        let writes0 = STATE_LOG.count();
        let a0 = crate::test_alloc::count();
        for k in 0..8 {
            script_on_tick(k);
        }
        let a1 = crate::test_alloc::count();
        assert_eq!(a1, a0, "ゲームのスレッドの経路で確保した");
        assert!(progress().fed >= 5, "押す道を通っていない");
        assert!(
            STATE_LOG.count() > writes0 && player_word(PLAYER_INV_TIMER_I) == 2,
            "状態の操作の経路を通っていない"
        );
        assert!(
            crate::shot_win::counters().placed > 0,
            "撮影の要求の経路を通っていない"
        );
        stop();
        crate::shot_win::tests::teardown();
    }

    #[test]
    fn a_watchers_local_tick_feeds_the_peeked_script_step_and_a_watching_tick_feeds_nothing() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(2);
        arm_io(fake_addrs(), feed_fn_addr(), local_fn_addr()).expect("arm_io");
        let mut io = real_io().expect("real_io");
        let script = || Engine::new(parse("scene=2 wait=1 p1=Z p2=ESC hold=3\n").unwrap());
        let mut e = script();
        let fed0 = FED.load(Ordering::SeqCst);
        let mut seen = Vec::new();
        for (k, action) in [
            Action::Local,
            Action::Menu,
            Action::RenderOnly,
            Action::Local,
            Action::Local,
        ]
        .into_iter()
        .enumerate()
        {
            FEED_N.store(0, Ordering::SeqCst);
            let (word, peeked) = {
                let mut g = NetGame::new(&mut io, Some(&mut e), fake_addrs());
                let w = g.peek_local_input();
                (w, g.peeked)
            };
            assert!(peeked.is_some(), "k={k}: 読みが台本を進めていない");
            let esc_now = word & keys::ESC != 0;
            assert_eq!(word & !keys::ESC, LOCAL_VALUE as u16, "k={k}");
            let ret = watch_return(action, Some(&mut e), &mut io, k as u32, peeked);
            seen.push((
                action,
                feed_log(),
                ret,
                matches!(e.status(), Status::Done),
                esc_now,
            ));
        }
        let (z, esc) = (u32::from(keys::Z), u32::from(keys::ESC));
        assert_eq!(
            seen.iter().map(|s| (s.0, s.2)).collect::<Vec<_>>(),
            vec![
                (Action::Local, 1),
                (Action::Menu, 1),
                (Action::RenderOnly, 2),
                (Action::Local, 1),
                (Action::Local, 0), // 台本が終わった Local は 0 = 元の飛び先
            ]
        );
        let feeds: Vec<_> = seen.iter().map(|s| s.1.clone()).collect();
        assert_eq!(
            feeds,
            vec![
                vec![(0, 0), (1, 0), (2, 0)],
                vec![],
                vec![],
                vec![(0, z), (1, esc), (2, z | esc)],
                vec![],
            ],
            "tick ごとの feed（観戦中の tick は空、台本の入力ワードが入るのは Local の押す tick だけ）"
        );
        assert_eq!(
            seen.iter().map(|s| s.3).collect::<Vec<_>>(),
            vec![false, false, false, false, true],
            "台本が 1 tick に 1 回ずつ進んでいない"
        );
        assert_eq!(
            seen.iter().map(|s| s.4).collect::<Vec<_>>(),
            vec![false, true, true, true, false],
            "読みに見える台本の Esc"
        );
        assert_eq!(
            FED.load(Ordering::SeqCst) - fed0,
            2,
            "feed した tick の数（Local のうち台本が入力ワードを持つ tick）"
        );
        let mut e2 = script();
        FEED_N.store(0, Ordering::SeqCst);
        assert_eq!(
            watch_return(Action::Local, Some(&mut e2), &mut io, 0, None),
            1
        );
        assert_eq!(feed_log(), vec![(0, 0), (1, 0), (2, 0)]);
        FEED_N.store(0, Ordering::SeqCst);
        assert_eq!(
            watch_return(Action::Local, Some(&mut e2), &mut io, 1, None),
            1
        );
        assert_eq!(feed_log(), vec![(0, z), (1, esc), (2, z | esc)]);
        assert_eq!(watch_return(Action::Local, None, &mut io, 2, None), 0);
        stop();
    }

    static FAKE_PLAYER: [AtomicU32; PLAYER_GAUGE as usize / 4 + 1] =
        [const { AtomicU32::new(0) }; PLAYER_GAUGE as usize / 4 + 1];
    static FAKE_PLAYER_SLOTS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];

    fn fake_player_slots() -> (u32, u32) {
        for off in [
            PLAYER_INV_TIMER_F,
            PLAYER_INV_TIMER_I,
            PLAYER_LIFE,
            PLAYER_GAUGE,
        ] {
            FAKE_PLAYER[off as usize / 4].store(0, Ordering::SeqCst);
        }
        FAKE_PLAYER_SLOTS[0].store(&FAKE_PLAYER as *const _ as usize as u32, Ordering::SeqCst);
        FAKE_PLAYER_SLOTS[1].store(0, Ordering::SeqCst);
        let a = |x: &'static AtomicU32| x as *const AtomicU32 as usize as u32;
        (a(&FAKE_PLAYER_SLOTS[0]), a(&FAKE_PLAYER_SLOTS[1]))
    }

    fn player_word(off: u32) -> u32 {
        FAKE_PLAYER[off as usize / 4].load(Ordering::SeqCst)
    }

    #[test]
    fn state_ops_are_written_through_the_real_hook() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        arm(
            parse("scene=3 wait=1 inv=p1 life1=2 gauge1=300\nscene=3 wait=3\n").unwrap(),
            fake_addrs(),
            feed_fn_addr(),
            local_fn_addr(),
        )
        .unwrap();
        let (s0, s1) = fake_player_slots();
        STATE_SLOTS[0].store(s0, Ordering::SeqCst);
        STATE_SLOTS[1].store(s1, Ordering::SeqCst);
        start().unwrap();
        let fields = || {
            [
                PLAYER_INV_TIMER_F,
                PLAYER_INV_TIMER_I,
                PLAYER_LIFE,
                PLAYER_GAUGE,
            ]
            .map(player_word)
        };
        let clear = || {
            for off in [
                PLAYER_INV_TIMER_F,
                PLAYER_INV_TIMER_I,
                PLAYER_LIFE,
                PLAYER_GAUGE,
            ] {
                FAKE_PLAYER[off as usize / 4].store(0, Ordering::SeqCst);
            }
        };
        let two = 2.0f32.to_bits();
        FAKE_DATA[1].store(1, Ordering::SeqCst);
        call_with_regs(fake_on_tick_addr());
        assert_eq!(fields(), [0; 4], "読み込み中に書いた");
        FAKE_DATA[1].store(0, Ordering::SeqCst);
        call_with_regs(fake_on_tick_addr());
        assert_eq!(fields(), [two, 2, 2, 300.0f32.to_bits()]);
        for t in [3, 4] {
            clear();
            call_with_regs(fake_on_tick_addr());
            assert_eq!(fields(), [two, 2, 0, 0], "tick {t}");
        }
        clear();
        call_with_regs(fake_on_tick_addr());
        assert_eq!(progress().state, RunState::Done);
        assert_eq!(SCRIPT_ON.load(Ordering::SeqCst), 0);
        assert_eq!(fields(), [0; 4], "台本が終わった tick に書いた");
        let text = STATE_LOG.text();
        assert!(text.contains("1P のライフ（+0xA8）に 2 を書いた"), "{text}");
        assert!(
            text.contains("1P のゲージ（+0x30388）に 300 を書いた"),
            "{text}"
        );
        assert!(text.contains("無敵を書いた tick 1P 3 / 2P 0"), "{text}");
        assert!(
            text.contains("書き込みに失敗した tick 1P 0 / 2P 0"),
            "{text}"
        );
        stop();
    }

    #[test]
    fn script_typed_fields_reach_the_room_input_only_while_the_gate_is_open() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _t = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pushed0 = TYPED_PUSHED.load(Ordering::Relaxed);
        let dropped0 = TYPED_DROPPED.load(Ordering::Relaxed);
        prepare_entry(3);
        arm(
            parse("scene=3 wait=1 bs=2 type=ab enter=1\nscene=3 wait=1 type=cd\nscene=3 wait=3\n")
                .unwrap(),
            fake_addrs(),
            feed_fn_addr(),
            local_fn_addr(),
        )
        .unwrap();
        start().unwrap();
        let mut out = [0u16; 16];
        crate::text_input_win::test_set_ring_open(true);
        call_with_regs(fake_on_tick_addr());
        let n = crate::text_input_win::drain(&mut out);
        assert_eq!(out[..n], [0x08, 0x08, 0x61, 0x62, 0x0D], "入る順・文字");
        crate::text_input_win::test_set_ring_open(false);
        call_with_regs(fake_on_tick_addr());
        assert_eq!(
            crate::text_input_win::drain(&mut out),
            0,
            "閉じているのに入った"
        );
        crate::text_input_win::test_set_ring_open(true);
        for _ in 0..6 {
            call_with_regs(fake_on_tick_addr());
        }
        assert_eq!(progress().state, RunState::Done);
        assert_eq!(
            crate::text_input_win::drain(&mut out),
            0,
            "台本が終わった後に入った"
        );
        assert_eq!(TYPED_PUSHED.load(Ordering::Relaxed) - pushed0, 5);
        assert_eq!(TYPED_DROPPED.load(Ordering::Relaxed) - dropped0, 2);
        crate::text_input_win::test_set_ring_open(false);
        stop();
    }

    #[test]
    fn state_ops_are_refused_through_the_real_hook_when_net_is_on() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        // SAFETY: 分岐は閉じている（`prepare_entry` の `stop`）。検査は REAL_HOOK_TESTS の下で 1 本ずつ。
        unsafe { *NET.0.get() = None };
        arm(
            parse("scene=3 wait=1 inv=p1 life1=2\nscene=3 wait=3\n").unwrap(),
            fake_addrs(),
            feed_fn_addr(),
            local_fn_addr(),
        )
        .unwrap();
        let (s0, s1) = fake_player_slots();
        STATE_SLOTS[0].store(s0, Ordering::SeqCst);
        STATE_SLOTS[1].store(s1, Ordering::SeqCst);
        NET_ON.store(1, Ordering::SeqCst);
        start().unwrap();
        for _ in 0..3 {
            call_with_regs(fake_on_tick_addr());
        }
        let words = [PLAYER_INV_TIMER_F, PLAYER_INV_TIMER_I, PLAYER_LIFE].map(player_word);
        assert_eq!(words, [0; 3], "ネットワークの実行で書いた");
        let text = STATE_LOG.text();
        assert!(
            text.contains("状態の操作を書かずに断った（ネットワーク（lockstep）の実行"),
            "{text}"
        );
        assert!(
            text.contains("1P のライフ 2 を、断ったので書かずに捨てた"),
            "{text}"
        );
        stop();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "higanrtool_script_win_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("logs")).unwrap();
        d
    }

    #[test]
    fn ini_missing_is_none_only_for_the_dll_folder() {
        use crate::net_config::Located;
        let d = temp_dir("ini_at");
        let at = |name: &str, env: Option<&'static str>| Located {
            path: d.join(name),
            from_env: env,
        };
        assert_eq!(read_ini_at(&at("HiganRTool.ini", None), None), Ok(None));
        let e = read_ini_at(&at("two_host.ini", Some("HIGANRTOOL_INI")), None).unwrap_err();
        assert!(e.contains("two_host.ini"), "{e}");
        std::fs::write(d.join("two_host.ini"), "[netplay]\nenable = 0\n").unwrap();
        assert_eq!(
            read_ini_at(&at("two_host.ini", Some("HIGANRTOOL_INI")), None),
            Ok(Some("[netplay]\nenable = 0\n".to_string()))
        );
        std::fs::write(d.join("bad.ini"), [0xFFu8, 0xFE]).unwrap();
        assert!(read_ini_at(&at("bad.ini", None), None).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn script_file_is_read_and_moved_without_replacing() {
        let d = temp_dir("move");
        let mut h = RealScriptHost {
            dll_dir: Some(d.clone()),
            run_path: Some(d.join("logs").join("dll_x.log")),
            exe_dir: Some(d.join("game")),
        };
        assert_eq!(h.read_script(), Ok(None));
        std::fs::write(d.join(SCRIPT_FILE), b"scene=1\n").unwrap();
        assert_eq!(h.read_script(), Ok(Some(b"scene=1\n".to_vec())));
        assert_eq!(h.move_script().as_deref(), Ok("dll_x_script.txt"));
        assert!(!d.join(SCRIPT_FILE).exists(), "元が残った");
        assert_eq!(
            std::fs::read(d.join("logs").join("dll_x_script.txt")).unwrap(),
            b"scene=1\n"
        );
        std::fs::write(d.join(SCRIPT_FILE), b"scene=2\n").unwrap();
        let e = h.move_script().unwrap_err();
        assert!(e.contains("移せない"), "{e}");
        assert_eq!(
            std::fs::read(d.join("logs").join("dll_x_script.txt")).unwrap(),
            b"scene=1\n"
        );
        assert!(d.join(SCRIPT_FILE).exists());
        let mut inside = RealScriptHost {
            dll_dir: Some(d.clone()),
            run_path: Some(d.join("logs").join("dll_y.log")),
            exe_dir: Some(d.clone()),
        };
        assert!(inside.read_script().is_err());
        assert!(inside.move_script().is_err());
        assert!(d.join(SCRIPT_FILE).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_decided_name_is_used_by_the_next_net_and_the_latest_one_is_remembered() {
        let d = temp_dir("namenext");
        let link = Arc::new(UiLink::new());
        let s = crate::net_config::from_text(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\nname = ini-synth\n",
        )
        .unwrap()
        .unwrap();
        let lazy = LazyNet {
            settings: s,
            code_dir: Some(d.clone()),
            ini_path: None,
            link: Arc::clone(&link),
        };
        assert_eq!(
            lazy.settings_now().name,
            "ini-synth",
            "決める前に名前が変わった"
        );
        let name = |t: &str| NameBuf::from_name(t).unwrap();
        link.send(higanr_core::net_menu::Cmd::Name(name("first-one")));
        assert_eq!(lazy.settings_now().name, "first-one");
        assert_eq!(
            lazy.settings.name, "ini-synth",
            "起動のときの設定を書き換えた"
        );
        link.send(higanr_core::net_menu::Cmd::Name(name("second")));
        write_player_name(&d, &link, name("first-one")).unwrap();
        assert_eq!(
            crate::player_name_file::read(&d).as_deref(),
            Some("second"),
            "古い名前が残った"
        );
        assert_eq!(lazy.settings_now().name, "second");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_room_name_decided_in_settings_is_remembered_and_a_stale_write_is_dropped() {
        let d = temp_dir("roomset");
        let name = |t: &str| RoomBuf::from_name(t).unwrap();
        assert_eq!(crate::room_last::read(&d), None);
        ROOM_SAVE_SEQ.store(7, Ordering::SeqCst);
        write_room_name(&d, 7, &name("decided-1")).unwrap();
        assert_eq!(crate::room_last::read(&d), Some(name("decided-1")));
        ROOM_SAVE_SEQ.store(8, Ordering::SeqCst);
        write_room_name(&d, 7, &name("stale")).unwrap();
        assert_eq!(
            crate::room_last::read(&d),
            Some(name("decided-1")),
            "古い回が新しい名を上書きした"
        );
        write_room_name(&d, 8, &name("latest")).unwrap();
        assert_eq!(crate::room_last::read(&d), Some(name("latest")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_remembered_name_is_read_only_for_the_in_game_menu_and_outside_the_game_folder() {
        let d = temp_dir("savedname");
        let code = d.join("code");
        std::fs::create_dir_all(&code).unwrap();
        let mut h = RealScriptHost {
            dll_dir: Some(d.clone()),
            run_path: Some(d.join("logs").join("dll_x.log")),
            exe_dir: Some(d.join("game")),
        };
        let game = |extra: &str| {
            crate::net_config::from_text(&format!(
                "[netplay]\nenable = 1\nbind = 127.0.0.1:0\n{extra}"
            ))
            .unwrap()
            .unwrap()
        };
        let menu = game("code_side = game\nclipboard = file\n");
        assert_eq!(h.saved_name(&menu), None);
        std::fs::write(code.join("player_name.txt"), "saved-synth\n").unwrap();
        assert_eq!(h.saved_name(&menu).as_deref(), Some("saved-synth"));
        std::fs::write(code.join("player_name.txt"), "\n").unwrap();
        assert_eq!(h.saved_name(&menu), None);
        std::fs::write(code.join("player_name.txt"), "saved-synth\n").unwrap();
        let offer = game("code_side = offer\n");
        assert_eq!(h.saved_name(&offer), None);
        let peer = game("peer = 127.0.0.1:7002\nsession = 0x1\n");
        assert_eq!(h.saved_name(&peer), None);
        let other = game("code_side = game\nclipboard = file\ncode_dir = other\n");
        assert_eq!(h.saved_name(&other), None);
        std::fs::create_dir_all(d.join("other")).unwrap();
        std::fs::write(d.join("other").join("player_name.txt"), "other-synth\n").unwrap();
        assert_eq!(h.saved_name(&other).as_deref(), Some("other-synth"));
        let mut inside = RealScriptHost {
            dll_dir: Some(d.clone()),
            run_path: None,
            exe_dir: Some(d.clone()),
        };
        assert_eq!(inside.saved_name(&menu), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn open_net_that_cannot_bind_tells_the_window() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let taken = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let d = temp_dir("not_started");
        let code = d.join("code");
        let text = format!("[netplay]\nenable = 1\nbind = 127.0.0.1:{port}\ncode_side = offer\n");
        let s = crate::net_config::from_text(&text).unwrap().unwrap();
        assert!(open_net(&s, 0, Some(code.clone()), None).is_err());
        let status = std::fs::read_to_string(code.join(net_code::STATUS_FILE)).unwrap();
        assert!(status.contains(net_code::show_not_started()), "{status}");
        let text = format!(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:{port}\npeer = 127.0.0.1:47002\nsession = 1\n"
        );
        let s = crate::net_config::from_text(&text).unwrap().unwrap();
        let other = d.join("other");
        assert!(open_net(&s, 0, Some(other.clone()), None).is_err());
        assert!(!other.exists());
        drop(taken);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn open_net_with_mapped_and_no_stun_answer_tells_the_window() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp_dir("mapped_no_stun");
        let code = d.join("code");
        let text = "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = answer\nstun = stun.test.example:3478\ncandidates = mapped\n";
        let s = crate::net_config::from_text(text).unwrap().unwrap();
        let e = open_net(&s, 0, Some(code.clone()), None).unwrap_err();
        assert!(
            e.contains("candidates = mapped") && e.contains("STUN は使わない"),
            "{e}"
        );
        let status = std::fs::read_to_string(code.join(net_code::STATUS_FILE)).unwrap();
        assert!(status.contains(net_code::show_not_started()), "{status}");
        assert!(!code.join(net_code::MY_CODE_FILE).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_the_game_menu_run_waits_for_ready() {
        for (side, want) in [
            ("code_side = game\nclipboard = file\n", true),
            ("code_side = offer\n", false),
            ("code_side = answer\n", false),
            ("peer = 127.0.0.1:47002\nsession = 1\n", false),
        ] {
            let text = format!("[netplay]\nenable = 1\nbind = 127.0.0.1:0\n{side}");
            let s = crate::net_config::from_text(&text).unwrap().unwrap();
            assert_eq!(play_config(&s, false).match_gate, want, "{side}");
        }
    }

    #[test]
    fn head_wait_reaches_the_play_config() {
        for (line, want) in [
            ("", netplay::HEAD_WAIT_CAP_US),
            ("head_wait = 1\n", netplay::HEAD_WAIT_CAP_US),
            ("head_wait = 0\n", 0),
        ] {
            let text =
                format!("[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = offer\n{line}");
            let s = crate::net_config::from_text(&text).unwrap().unwrap();
            assert_eq!(play_config(&s, false).head_wait_cap_us, want, "{line:?}");
        }
    }

    #[test]
    fn the_watch_scan_borrows_only_a_watch_box_on_the_game_thread_of_a_running_net() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        let peer = std::net::SocketAddr::from(([127, 0, 0, 1], 9));
        let (mut net, _th) =
            net_udp::open(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), &[peer]).unwrap();
        let cfg = higanr_core::link::Config::single(
            higanr_core::handshake::Config {
                session_id: 1,
                role_nonce: 1,
                latency: 2,
                name: higanr_core::handshake::Name::new("t"),
                host_seat: higanr_core::wire::Seat::P1,
            },
            higanr_core::auth::Secret([1; 16]),
            peer,
            higanr_core::seal::KeySeed([0x4B; 32]),
        );
        let np = NetPlay::new(cfg, netplay::Config::default(), &mut net);
        // SAFETY: 分岐は閉じている（`prepare_entry` の `stop`）。ゲームのスレッドは居ない。検査は REAL_HOOK_TESTS の下で 1 本ずつ。
        unsafe { *NET.0.get() = Some(Slot::Play(Box::new(NetSlot { np, net }))) };
        // SAFETY: 引数の無い呼び出し。
        let tid = unsafe { GetCurrentThreadId() };
        let lent = || watch_slot_for_scan().is_some();
        NET_ON.store(1, Ordering::SeqCst);
        GAME_TID.store(tid, Ordering::SeqCst);
        assert!(!lent(), "Play の入れ物を貸した");
        // Watch の入れ物（対照）。
        let (wnet, _wth) =
            net_udp::open_for_code(std::net::SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let wp = WatchPlay::new(
            std::sync::Arc::new(WatchSlot::new()),
            WatchConfig {
                wait_budget_us: 16_000,
                margin_frames: WATCH_MARGIN_FRAMES,
                require_net_selection: false,
            },
        );
        let wslot = Box::new(WatchNetSlot {
            wp,
            net: wnet,
            shots: Default::default(),
        });
        // SAFETY: 同上。上の借用は残っていない（`lent` は真偽だけを返す）。
        unsafe { *NET.0.get() = Some(Slot::Watch(wslot)) };
        assert!(
            lent(),
            "Watch の入れ物でそろっているのに貸さない（検査が素通り）"
        );
        for other in [tid.wrapping_add(1), 0] {
            GAME_TID.store(other, Ordering::SeqCst);
            assert!(!lent(), "GAME_TID = {other} でも貸した");
        }
        GAME_TID.store(tid, Ordering::SeqCst);
        NET_ON.store(0, Ordering::SeqCst);
        assert!(!lent(), "NET_ON 0 で貸した");
        // SAFETY: 同上。
        unsafe { *NET.0.get() = None };
        GAME_TID.store(0, Ordering::SeqCst);
    }

    #[test]
    fn the_scan_call_is_placed_for_a_watcher_run_at_startup_or_at_open_and_players_install_whatever_they_wish()
     {
        use crate::rollback_play::tests::FakeCode;
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let reset = || {
            crate::rollback_win::set_scan_original(0);
            crate::rollback_win::set_scan_handler(None);
        };
        reset();
        crate::rollback_win::TEST_OUTPUT_RANGES_OK.store(true, Ordering::SeqCst);
        let settings = |side: &str| {
            crate::net_config::from_text(&format!(
                "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = {side}\nmode = delay\n"
            ))
            .unwrap()
            .unwrap()
        };
        let mut host = RealScriptHost {
            dll_dir: None,
            run_path: None,
            exe_dir: None,
        };
        let forbidden = crate::th09_addr::FORBIDDEN;
        // rollback のときはどの役でも書く望み（対照: 観戦者の rollback の実行は今までどおり）。
        use higanr_core::wire::Mode as M;
        for side in ["offer", "answer", "game"] {
            assert_eq!(install_wish(&settings(side)), M::Rollback, "{side}");
        }
        assert_eq!(install_wish(&settings("spectate")), M::Delay);
        let rb_spectator = crate::net_config::from_text(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = spectate\nmode = rollback\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(install_wish(&rb_spectator), M::Rollback);
        let mut f = FakeCode::new();
        let note = arm_watch_scan_at_open_with(&mut f);
        assert!(note.starts_with("走査の call を書いた"), "{note}");
        assert_eq!(f.writes.len(), 1, "{note}");
        assert!(crate::rollback_win::scan_hooked());
        assert!(crate::rollback_win::scan_handler_is_set());
        let note = arm_watch_scan_at_open_with(&mut f);
        assert!(note.contains("書いてある"), "{note}");
        assert_eq!(f.writes.len(), 1, "2 回目で書き足した");
        reset();
        let mut f = FakeCode::new();
        let mut out = Vec::new();
        host.arm_rollback(&settings("spectate"), &mut f, forbidden, &mut out);
        assert_eq!(
            f.writes.len(),
            1,
            "観戦者の実行で書かない: {}",
            String::from_utf8_lossy(&out)
        );
        assert!(crate::rollback_win::scan_hooked());
        assert!(crate::rollback_win::scan_handler_is_set());
        reset();
        crate::rollback_win::TEST_OUTPUT_RANGES_OK.store(false, Ordering::SeqCst);
        let mut f = FakeCode::new();
        let note = arm_watch_scan_at_open_with(&mut f);
        assert!(note.starts_with("走査の call を書けなかった"), "{note}");
        assert!(f.writes.is_empty(), "文字の数の欄の頁を確かめずに書いた");
        reset();
    }

    #[test]
    fn the_head_wait_hook_borrows_the_box_only_on_the_game_thread_of_a_running_play_net() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        let peer = std::net::SocketAddr::from(([127, 0, 0, 1], 9));
        let (mut net, _th) =
            net_udp::open(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), &[peer]).unwrap();
        let cfg = higanr_core::link::Config::single(
            higanr_core::handshake::Config {
                session_id: 1,
                role_nonce: 1,
                latency: 2,
                name: higanr_core::handshake::Name::new("t"),
                host_seat: higanr_core::wire::Seat::P1,
            },
            higanr_core::auth::Secret([1; 16]),
            peer,
            higanr_core::seal::KeySeed([0x4B; 32]),
        );
        let np = NetPlay::new(cfg, netplay::Config::default(), &mut net);
        // SAFETY: 分岐は閉じている（`prepare_entry` の `stop`）。ゲームのスレッドは居ない。検査は REAL_HOOK_TESTS の下で 1 本ずつ。
        unsafe { *NET.0.get() = Some(Slot::Play(Box::new(NetSlot { np, net }))) };
        // SAFETY: 引数の無い呼び出し。
        let tid = unsafe { GetCurrentThreadId() };
        let lent = || head_wait_slot().is_some();
        // 対照: 4 つがそろえば貸す。
        NET_ON.store(1, Ordering::SeqCst);
        GAME_TID.store(tid, Ordering::SeqCst);
        RB_HELD.store(0, Ordering::SeqCst);
        assert!(lent(), "そろっているのに貸さない（検査が素通り）");
        for other in [tid.wrapping_add(1), 0] {
            GAME_TID.store(other, Ordering::SeqCst);
            assert!(!lent(), "GAME_TID = {other} でも貸した");
            head_wait_hook();
        }
        GAME_TID.store(tid, Ordering::SeqCst);
        RB_HELD.store(1, Ordering::SeqCst);
        assert!(!lent(), "RB_HELD の間に貸した");
        RB_HELD.store(0, Ordering::SeqCst);
        NET_ON.store(0, Ordering::SeqCst);
        assert!(!lent(), "NET_ON 0 で貸した");
        NET_ON.store(1, Ordering::SeqCst);
        // SAFETY: 同上。上の借用は残っていない（`lent` は真偽だけを返す）。
        unsafe { *NET.0.get() = None };
        assert!(!lent(), "入れ物が無いのに貸した");
        NET_ON.store(0, Ordering::SeqCst);
        GAME_TID.store(0, Ordering::SeqCst);
    }

    /// 偽のメモリ（`fake_mem`）の上のグローバル状態語。アドレスは本物と同じにする（立てる前に、照合した 9 バイトの `testb` に埋まったアドレスと
    /// 書く語のアドレスを結び付けるので、ずらすと断る）。
    const FAKE_STATE: u32 = crate::th09_addr::GLOBAL_STATE;

    fn skip_reset() {
        SKIP_WATCH.store(0, Ordering::SeqCst);
        for a in [&SKIP_SET, &SKIP_ALREADY, &SKIP_CLEAR_RETRY]
            .into_iter()
            .chain(SKIP_REFUSED.iter())
            .chain(SKIP_CLEARED.iter())
        {
            a.store(0, Ordering::SeqCst);
        }
        SKIP_SITE_AT.store(SAVE_SKIP_SITE.addr, Ordering::SeqCst);
    }

    #[test]
    fn save_skip_wiring_sets_checks_and_clears_through_own_memory() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let addrs_before = ADDRS[3].load(Ordering::SeqCst);
        ADDRS[3].store(FAKE_STATE, Ordering::SeqCst);
        fake_mem::enable();
        skip_reset();
        let good = SAVE_SKIP_SITE.expected;
        let mut bad = [0u8; 9];
        bad.copy_from_slice(good);
        bad[8] ^= 0x01;
        SKIP_SITE_AT.store(good.as_ptr() as usize as u32, Ordering::SeqCst);
        let word = || fake_mem::get(FAKE_STATE).unwrap();
        fake_mem::set(FAKE_STATE, 0x0001_0004);

        assert_eq!(save_skip_mark(false), Marked::Refused(SkipRefuse::NotSaved));
        assert_eq!(word(), 0x0001_0004);
        assert_eq!(watch_of(SKIP_WATCH.load(Ordering::SeqCst)), Watch::Idle);
        SKIP_SITE_AT.store(bad.as_ptr() as usize as u32, Ordering::SeqCst);
        assert_eq!(
            save_skip_mark(true),
            Marked::Refused(SkipRefuse::SiteMismatch)
        );
        assert_eq!(word(), 0x0001_0004, "照合が外れたのに書いた");
        SKIP_SITE_AT.store(good.as_ptr() as usize as u32, Ordering::SeqCst);
        for off in [1u32, 4] {
            let shifted = FAKE_STATE + off;
            ADDRS[3].store(shifted, Ordering::SeqCst);
            fake_mem::set(shifted, 0x0001_0004);
            assert_eq!(
                save_skip_mark(true),
                Marked::Refused(SkipRefuse::SiteTarget),
                "ずれ {off}"
            );
            assert_eq!(
                fake_mem::get(shifted),
                Some(0x0001_0004),
                "ずれ {off} の語に書いた"
            );
            assert_eq!(watch_of(SKIP_WATCH.load(Ordering::SeqCst)), Watch::Idle);
        }
        ADDRS[3].store(FAKE_STATE, Ordering::SeqCst);
        assert_eq!(word(), 0x0001_0004);
        assert_eq!(save_skip_mark(true), Marked::Set);
        assert_eq!(word(), 0x0001_0004 | CONTINUED_BIT);
        save_skip_step(true, None);
        save_skip_step(false, Some(1));
        assert_eq!(word() & CONTINUED_BIT, CONTINUED_BIT, "15 より前に落とした");
        save_skip_step(false, Some(14));
        save_skip_step(false, Some(15));
        assert_eq!(word() & CONTINUED_BIT, CONTINUED_BIT);
        save_skip_step(false, Some(1));
        assert_eq!(word(), 0x0001_0004, "タイトルで落とさない");
        assert_eq!(watch_of(SKIP_WATCH.load(Ordering::SeqCst)), Watch::Idle);
        assert_eq!(save_skip_mark(true), Marked::Set);
        save_skip_step(false, None);
        save_skip_step(true, None);
        assert_eq!(word(), 0x0001_0004, "次の試合で落とさない");
        assert_eq!(save_skip_mark(true), Marked::Set);
        save_skip_abandon();
        assert_eq!(word(), 0x0001_0004, "パニックで落とさない");
        fake_mem::set(FAKE_STATE, 0x0000_2004);
        assert_eq!(save_skip_mark(true), Marked::AlreadySet);
        for (m, s) in [(false, Some(14)), (false, Some(1)), (true, None)] {
            save_skip_step(m, s);
        }
        assert_eq!(word(), 0x0000_2004, "ゲームの印を落とした");
        let text = save_skip_text();
        for want in [
            "立てた 3 回",
            "もう立っていた 1 回",
            "この試合の自動保存が保存関数を呼んでいない 1 回",
            "分かれ道のバイト列が期待と違う 1 回",
            "分かれ道が見るアドレス・ビットが書く語と違う 2 回",
            "タイトル 1 回・次の試合 1 回",
        ] {
            assert!(text.contains(want), "{want}: {text}");
        }
        skip_reset();
        fake_mem::clear();
        ADDRS[3].store(addrs_before, Ordering::SeqCst);
    }

    #[test]
    fn save_quit_tick_marks_only_when_the_autosave_ran() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let addrs_before = ADDRS[3].load(Ordering::SeqCst);
        ADDRS[3].store(FAKE_STATE, Ordering::SeqCst);
        fake_mem::enable();
        skip_reset();
        SKIP_SITE_AT.store(
            SAVE_SKIP_SITE.expected.as_ptr() as usize as u32,
            Ordering::SeqCst,
        );
        let peer = std::net::SocketAddr::from(([127, 0, 0, 1], 9));
        let (mut net, _th) =
            net_udp::open(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), &[peer]).unwrap();
        let cfg = higanr_core::link::Config::single(
            higanr_core::handshake::Config {
                session_id: 1,
                role_nonce: 1,
                latency: 2,
                name: higanr_core::handshake::Name::new("t"),
                host_seat: higanr_core::wire::Seat::P1,
            },
            higanr_core::auth::Secret([1; 16]),
            peer,
            higanr_core::seal::KeySeed([0x4B; 32]),
        );
        let mut np = NetPlay::new(cfg, netplay::Config::default(), &mut net);
        let word = || fake_mem::get(FAKE_STATE).unwrap();
        let not_saved = SkipRefuse::ALL
            .iter()
            .position(|&r| r == SkipRefuse::NotSaved)
            .unwrap();
        fake_mem::set(FAKE_STATE, 0x0001_0004);

        crate::replay_save_win::set_seg_saved_for_test(true, true);
        save_quit_tick(&mut np);
        assert_eq!(word(), 0x0001_0004, "頼みが無いのに立てた");
        assert_eq!(SKIP_SET.load(Ordering::SeqCst), 0);
        assert_eq!(SKIP_REFUSED[not_saved].load(Ordering::SeqCst), 0);
        for (armed, saved) in [(false, false), (false, true), (true, false)] {
            crate::replay_save_win::set_seg_saved_for_test(armed, saved);
            assert!(!crate::replay_save_win::saved_this_seg(), "検査の前提");
            np.set_save_quit();
            save_quit_tick(&mut np);
            assert_eq!(
                word(),
                0x0001_0004,
                "入っている {armed}・呼んだ {saved} で立てた"
            );
            assert_eq!(watch_of(SKIP_WATCH.load(Ordering::SeqCst)), Watch::Idle);
        }
        assert_eq!(
            SKIP_REFUSED[not_saved].load(Ordering::SeqCst),
            3,
            "呼んでいない試合の頼みを数えていない（配線が `mark` へ届いていない）"
        );
        crate::replay_save_win::set_seg_saved_for_test(true, true);
        np.set_save_quit();
        save_quit_tick(&mut np);
        assert_eq!(word(), 0x0001_0004 | CONTINUED_BIT, "呼んだ試合で立てない");
        assert_eq!(SKIP_SET.load(Ordering::SeqCst), 1);
        assert_eq!(watch_of(SKIP_WATCH.load(Ordering::SeqCst)), Watch::Leaving);
        save_quit_tick(&mut np);
        assert_eq!(
            SKIP_SET.load(Ordering::SeqCst),
            1,
            "1 回の頼みで 2 回立てた"
        );

        crate::replay_save_win::set_seg_saved_for_test(false, false);
        skip_reset();
        fake_mem::clear();
        ADDRS[3].store(addrs_before, Ordering::SeqCst);
    }

    #[test]
    fn watch_save_quit_tick_marks_for_a_watcher_even_without_an_autosave() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let addrs_before = ADDRS[3].load(Ordering::SeqCst);
        ADDRS[3].store(FAKE_STATE, Ordering::SeqCst);
        fake_mem::enable();
        skip_reset();
        SKIP_SITE_AT.store(
            SAVE_SKIP_SITE.expected.as_ptr() as usize as u32,
            Ordering::SeqCst,
        );
        let mut wp = WatchPlay::new(
            Arc::new(WatchSlot::new()),
            WatchConfig {
                wait_budget_us: 16_000,
                margin_frames: WATCH_MARGIN_FRAMES,
                require_net_selection: true,
            },
        );
        let word = || fake_mem::get(FAKE_STATE).unwrap();
        fake_mem::set(FAKE_STATE, 0x0001_0004);

        crate::replay_save_win::set_seg_saved_for_test(false, false);
        watch_save_quit_tick(&mut wp);
        assert_eq!(word(), 0x0001_0004, "頼みが無いのに立てた");
        assert_eq!(SKIP_SET.load(Ordering::SeqCst), 0);
        let mut bad = [0u8; 9];
        bad.copy_from_slice(SAVE_SKIP_SITE.expected);
        bad[8] ^= 0x01;
        SKIP_SITE_AT.store(bad.as_ptr() as usize as u32, Ordering::SeqCst);
        wp.set_save_quit_for_test();
        watch_save_quit_tick(&mut wp);
        assert_eq!(word(), 0x0001_0004, "照合が外れたのに書いた");
        SKIP_SITE_AT.store(
            SAVE_SKIP_SITE.expected.as_ptr() as usize as u32,
            Ordering::SeqCst,
        );
        assert!(!crate::replay_save_win::saved_this_seg(), "検査の前提");
        wp.set_save_quit_for_test();
        watch_save_quit_tick(&mut wp);
        assert_eq!(
            word(),
            0x0001_0004 | CONTINUED_BIT,
            "自動保存の無い観戦者で立てない"
        );
        assert_eq!(SKIP_SET.load(Ordering::SeqCst), 1);
        assert_eq!(watch_of(SKIP_WATCH.load(Ordering::SeqCst)), Watch::Leaving);
        watch_save_quit_tick(&mut wp);
        assert_eq!(
            SKIP_SET.load(Ordering::SeqCst),
            1,
            "1 回の頼みで 2 回立てた"
        );

        skip_reset();
        fake_mem::clear();
        ADDRS[3].store(addrs_before, Ordering::SeqCst);
    }

    #[test]
    fn save_skip_is_watched_at_the_entry_and_dropped_on_panic() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(2);
        arm(
            parse("scene=2 wait=100 p1=Z\n").unwrap(),
            fake_addrs(),
            feed_fn_addr(),
            local_fn_addr(),
        )
        .unwrap();
        start().unwrap();
        skip_reset();
        let st = &FAKE_DATA[2];
        st.store(0x0001_2004, Ordering::SeqCst);
        SKIP_WATCH.store(watch_code(Watch::Away), Ordering::SeqCst);
        script_on_tick(0);
        assert_eq!(
            st.load(Ordering::SeqCst),
            0x0001_0004,
            "入口の頭で落とさない"
        );
        assert_eq!(watch_of(SKIP_WATCH.load(Ordering::SeqCst)), Watch::Idle);
        st.store(0x0000_2004, Ordering::SeqCst);
        script_on_tick(1);
        assert_eq!(st.load(Ordering::SeqCst), 0x0000_2004);
        st.store(0x0001_2004, Ordering::SeqCst);
        SKIP_WATCH.store(watch_code(Watch::Passed), Ordering::SeqCst);
        PANIC_NEXT.store(true, Ordering::SeqCst);
        assert_eq!(script_on_tick(2), 0);
        assert_eq!(SCRIPT_ON.load(Ordering::SeqCst), 0, "検査の前提");
        assert_eq!(
            st.load(Ordering::SeqCst),
            0x0001_0004,
            "パニックで落とさない"
        );
        stop();
        skip_reset();
    }

    #[test]
    fn the_watch_target_is_the_picked_row_or_the_typed_name_and_never_a_stale_pick() {
        use higanr_core::net_menu::Cmd;
        use higanr_core::room::RoomName;
        use higanr_core::watch_list::{WatchName, WatchRow};
        let link = UiLink::new();
        assert!(watch_target(&link).is_err(), "何も置いていない");
        // 打ったルーム名: その名から観戦用のルームを導く。
        let typed = RoomBuf::from_name("Typed-Room").unwrap();
        link.send(Cmd::Spectate(typed));
        let (room, shown) = watch_target(&link).unwrap();
        assert_eq!(room, typed.parse().unwrap().spectate_of());
        assert_eq!(shown, "Typed-Room");
        // 一覧から選んだ行: ハッシュから観戦用のルーム名を戻し、左上の名は「1P vs 2P」。ホストが割り当ての名から導く名と同じ。
        let q = RoomName::parse_server_rules("q.0123456789abcdef0123456789abcd").unwrap();
        let row = WatchRow {
            hash: [0x60, 0x4d, 0x50, 0x2a, 0x39, 0x81, 0x06, 0x5f],
            p1: WatchName::parse("Alice").unwrap(),
            p2: WatchName::parse("Bob").unwrap(),
            mode: higanr_core::wire::Mode::Rollback,
            delay: 1,
            watchers: 0,
            cap: 10,
        };
        link.send(Cmd::SpectateListed(row));
        let (room, shown) = watch_target(&link).unwrap();
        assert_eq!(room.as_str(), "w.604d502a3981065f");
        assert_eq!(room, q.spectate_of());
        assert_eq!(shown, "Alice vs Bob");
        // 打ち直したら、古い選びではなく打った名。
        link.send(Cmd::Spectate(typed));
        let (room, _) = watch_target(&link).unwrap();
        assert_eq!(room, typed.parse().unwrap().spectate_of());
    }
}
