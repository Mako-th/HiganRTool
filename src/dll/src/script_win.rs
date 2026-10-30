//! 台本を動かす層。ゲームのスレッドの層と台本のファイルの層に分かれる（unsafeはここにもある。1つずつ`// SAFETY:`）。
//!
//! - 入力の入口（`stub.rs`の`input_wait_entry`）は`SCRIPT_ON`が0でなければ、tickの記録を書いた後に`script_on_tick(k)`を呼ぶ。
//!   戻り値が0なら元の飛び先（poll_all）へ進む。1ならpoll_allを呼ばずに戻る。中は全部`catch_unwind`で囲む。
//!   判断は`scripted::tick`（pure）にだけ書く。ここが持つのは、ゲームのメモリの読み・ゲームの関数の呼び出し・台本のコンテナ・進み具合のアトミック変数・feedのリングバッファへの書き込み・撮影の要求だけ。
//!   ゲームのスレッドの経路では、確保・ファイル・ロック待ち・`unwrap`をしない。
//! - 台本のファイル: DLLのフォルダの`script.txt`。読んだら本ログの隣の`<回の名前>_script.txt`へ上書きせずに移す。
//!   移すには`MoveFileExW`をフラグ0で呼ぶ。`std::fs::rename`は上書きのフラグで呼ぶので使わない。
//!   ローダーの`--script` / `--ini`が環境変数で渡したパスがあればそちらを読む。渡された台本は移さない（2つのゲームがそれぞれのコピーを持てるように）。
//! - **ネットワーク**: ネットワークを組み込んだ実行では、入口の分岐は台本とネットワークの両方の入口になる。
//!   `script_on_tick`はまずネットワークの判断（`netplay::NetPlay::on_input`）を呼ぶ。
//!   `Local`なら台本の経路（終わっていれば0）、`Zero` / `Played`なら1、`RenderOnly`なら2を返す。
//!   ネットワークのセッション（`NET`）に触るのはゲームのスレッドだけ。台本が無くてもネットワークは開ける（`arm_io`）。
//!   `after_script = keep`なら台本が最後の段階まで押してもネットワークを閉じない。
//! - **観戦者の側**: 「ルームで観戦」を選ぶと、開くスレッドが観戦者のネットワークを作る（`open_watch_net`）。
//!   ゲームのスレッドが郵便受け`PENDING`から取り出して**後から**組み込む（`install_placed`）。
//!   待っているだけの対戦のセッションがあるときに選んだ場合は、先にそのセッションを手放し（`release_slot`）、ワーカーのスレッドが後始末してから続けて開く（`SPECTATE_AFTER`）。
//!   次が全部揃ったらセッションを手放す（`watch_teardown`）: 観戦が終わった・マッチの外へ戻った・ルームのスレッドが書き終えた。
//! - **マッチの状態の操作**: `tick_inner`はどの経路を通ったtickでも最後に`state_tick`で台本の無敵・ライフ・ゲージを書く（判断は`state_ops::apply`）。
//!   Playerはヒープにあるので、例外を出さない手段（`OwnProcess` / `WriteProcessMemory`）で読み書きする。
//!   ネットワークか巻き戻しの実験が動いている実行では書かずに断る（`state_ops::Refuse`）。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "script_win.rs は 32 ビット（i686）専用（ゲームの関数と 32 ビットのアドレスを前提にしている）"
);

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};
use std::ffi::OsStr;
use std::net::Ipv4Addr;
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

use higanr_core::guard::Scope;
use higanr_core::lan_addr::{AddrBuf, LanAddr};
use higanr_core::lan_proto::Ver;
use higanr_core::lang::Lang;
use higanr_core::mode_pick::Shape;
use higanr_core::net_menu::{CodeState, Conn, Event, Note, RoomItem, sanitize_room};
use higanr_core::player_name::NameBuf;
use higanr_core::room_input::InitialFrom;
use higanr_core::save_skip::{
    self, Clear, Marked, Refuse as SkipRefuse, Side as SkipSide, SkipMem, Watch,
};
use higanr_core::wire::Seat;

use crate::adapters_win;
use crate::clipboard::{Clipboard, FileClipboard};
use crate::clipboard_win::SystemClipboard;
use crate::lan_link::{self, LanBridge, LanClip, LanEnv, LanRole};
use crate::net_code;
use crate::net_code_game::{self, GameUi};
use crate::net_config::{
    AfterScript, Bind, ClipboardKind, CodeSide, DEFAULT_BIND_PORT, NetSettings,
};
use crate::net_game::{self, NetGame, NetIo};
use crate::net_portrait::PortraitCall;
use crate::net_room::RoomOptions;
use crate::net_udp::{self, GameNet, RecvThread, WatchUpnpPlan};
use crate::net_ui::{self, BOARD, HudFacts, LanAsk, NetPhase, NetUi, UiFacts, UiLink, UiOut};
use crate::net_watch::{NET_FRAMES, NET_STATUS, NetSnapshot};
use crate::netplay::{self, Action, NetPlay};
use crate::scripted::{self, GameAddrs, Progress, RunState, TickIo};
use crate::session::ScriptHost;
use crate::shot_win;
use crate::spectate_host;
use crate::spectate_join::{self, JoinKit, JoinSource, Publisher};
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

/// 台本が動いているかどうか。入力の入口のアセンブリが`cmp dword ptr [SCRIPT_ON], 0`で見る。
/// 0 → 1にするのは読み取りのスレッドだけ（`start`）。
/// 1 → 0にするのはゲームのスレッド（台本が終わった・止まった・パニック）と検査の後始末。
pub static SCRIPT_ON: AtomicU32 = AtomicU32::new(0);

/// ゲームのスレッドが読むアドレス（`GameAddrs`の並び）と呼ぶ関数のアドレス（feed・ローカル入力）。
/// `arm`が`SCRIPT_ON`を1にする前に設定する。
static ADDRS: [AtomicU32; 10] = [const { AtomicU32::new(0) }; 10];
static FEED_FN: AtomicU32 = AtomicU32::new(0);
static LOCAL_FN: AtomicU32 = AtomicU32::new(0);
/// 台本の状態の操作がたどるPlayerへのポインタを置くスロット（1P, 2P）。`arm_io`が花映塚のアドレスを設定する（検査は偽のスロットへ設定し直す）。
static STATE_SLOTS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];

struct EngineCell(UnsafeCell<Option<Engine>>);
// SAFETY: 中身に触るのは2か所だけで、同時には触らない。
// 読み取りのスレッドの`arm`は`SCRIPT_ON`が0の間だけ設定する。
// 本番では2つのフックを書く前に1回だけ。検査では`REAL_HOOK_TESTS`のロックの下で1本ずつ。
// 以後はゲームのスレッドだけが触る。`start`はReleaseで、ゲームのスレッドはAcquireで読んでから触る。
unsafe impl Sync for EngineCell {}
static ENGINE: EngineCell = EngineCell(UnsafeCell::new(None));
/// 台本のコンテナに台本が入っているかどうか（`arm`が1、`arm_io`が0）。台本の付いた実行は本物のキーを読まない。
/// ただしrollbackのフックがコンテナを`&mut`で持っている間は中を覗けないので、別に持つ（`shot_tick`）。
/// 書くのは`SCRIPT_ON`が0の間の読み取りのスレッドだけ。
static HAS_SCRIPT: AtomicU32 = AtomicU32::new(0);

static STATE: AtomicU32 = AtomicU32::new(0);
static STEP: AtomicU32 = AtomicU32::new(0);
static LINE: AtomicU32 = AtomicU32::new(0);
static TICKS: AtomicU32 = AtomicU32::new(0);
static FED: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
pub(crate) static PANIC_NEXT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// 検査だけ: 0でなければ次の`script_on_tick`はこの値 − 1 を返す（入力の入口のアセンブリの戻り値の経路を見る）。
#[cfg(test)]
pub(crate) static FORCE_RET: AtomicU32 = AtomicU32::new(0);
/// 検査だけ: `match_start_on`が呼ばれた回数（開始のフックの入口が`NET_ON`を見て呼ぶかを数える）。
#[cfg(test)]
pub(crate) static MATCH_ON_CALLS: AtomicU32 = AtomicU32::new(0);

// ---- ネットワーク ----

/// ネットワークが動いているかどうか。開始のフックの入口のアセンブリが`cmp dword ptr [NET_ON], 0`で見る。
/// 0 → 1にするのは読み取りのスレッドだけ（`start_net`）。
/// 1 → 0にするのはゲームのスレッド（パニック）。
pub static NET_ON: AtomicU32 = AtomicU32::new(0);
static SET_STATE_FN: AtomicU32 = AtomicU32::new(0);
/// 立ち絵の3本の関数の入口（`net_portrait::PortraitFn::index`の順）。
/// `session::run`が3本のバイト列を全部照合して合ったときだけ`arm_portrait`が設定する（0のままなら呼ばない）。
static PORTRAIT_FNS: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
/// 素のpoll `0x0042B850`の入口。起動時に`session::run`が先頭8バイトを照合して合ったときだけ`arm_poll`が設定する。
/// 0のままならローカル入力は `LOCAL_FN` = `0x0042BE40` で読む。
static POLL_FN: AtomicU32 = AtomicU32::new(0);
static GAME_TID: AtomicU32 = AtomicU32::new(0);
pub static MATCH_FOREIGN: AtomicU32 = AtomicU32::new(0);
pub static NET_PANICKED: AtomicU32 = AtomicU32::new(0);
/// 1は、rollbackのモードのフックがネットワークのセッション`NET`と台本のコンテナ`ENGINE`を`&mut`で持っている間を表す。
/// フックが呼ぶ走査の中で入れ子に走る入力の入口と開始のフックは、その間2つのコンテナに触らない。
/// 入力の入口は、指図（`rollback_play_win::input_hook`）があればfeedして戻り、無ければpoll_allへ渡す。
/// 開始のフックは`MATCH_NESTED`に数えて戻る。
pub static RB_HELD: AtomicU32 = AtomicU32::new(0);
/// フックがコンテナを持っている間に来た開始のフック（来ない見込み）。
pub static MATCH_NESTED: AtomicU32 = AtomicU32::new(0);
/// フックがコンテナを持っている間に指図が無いまま入力の入口へ来た回数（来ない見込み）。
/// 来たら本物のキーボードをrollbackのtickに入れないよう、poll_allを呼ばずに戻した。
pub static RB_STRAY: AtomicU32 = AtomicU32::new(0);
/// 走査のラッパーのフックを通ったフレームの数（本ログ用のカウンタ`rollback_play_win::publish_summary`を数フレームに1回にする）。
static RB_FRAMES: AtomicU32 = AtomicU32::new(0);
/// マッチをやめた回数（`NetPlay`のカウンタをフックがコピーする）。
/// 増えたのを見たら、その場で本ログ用のカウンタもコピーする。
/// やめた後はフックがカウンタをコピーしなくなるので、ここでコピーしないと要約がやめる前の値のまま残る。
static RB_ABORTS: AtomicU32 = AtomicU32::new(0);
/// 最初にマッチをやめた経路（`abort_code`）と、そのときまでに確定したフレームの数。
static RB_ABORT_FIRST: AtomicU32 = AtomicU32::new(0);
static RB_ABORT_FRAME: AtomicU32 = AtomicU32::new(0);
/// rollbackのマッチの間にフックがコンテナ（`real_io` / `with_play`）を借りられず、走査を普段どおりに回した回数（来ない見込み）。
static RB_BYPASS: AtomicU32 = AtomicU32::new(0);

/// マッチをやめたのを見たら（`NetPlay`のカウンタがコピーした値より多い）経路をコピーする。本ログ用のカウンタもその場でコピーする。
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
/// 台本が最後の段階まで押した後もネットワークを閉じない（iniの`after_script = keep`）。`open_net`が設定し、ゲームのスレッドが`net_tick`で読む。
static KEEP_AFTER_SCRIPT: AtomicU32 = AtomicU32::new(0);

pub struct NetSlot {
    pub np: NetPlay,
    pub net: GameNet,
}

/// 観戦者のセッション（iniの`code_side = spectate`。対戦のセッション`NetSlot`の代わりに組み込む）。
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
// SAFETY: 読み取りのスレッドの`arm_net` / `arm_watch`は`NET_ON`と`SCRIPT_ON`が0の間だけ設定する（本番では3つのフックを書く前に1回だけ）。
// 以後はゲームのスレッドだけが触る。入れ子にならない。開始のフックが別のスレッドから来たら触らない（`GAME_TID`）。
// `start_net`はReleaseで、ゲームのスレッドはAcquireで読んでから触る。
unsafe impl Sync for NetCell {}
static NET: NetCell = NetCell(UnsafeCell::new(None));

// ---- ゲーム内のメニュー（iniの`code_side = game`）----
//
// - 起動時（`arm_game_ui`）: 受信ソケット・STUN・監視の処理は**開かない**。用意するのは、メニューのコンテナ（`UI`）とネットワークを開くのに要るもの（`LAZY`）だけ。
//   入力の入口は有効にする（`UI_ON`）。ただし画面6でメニューが入力を占有するだけで、描くものは無い。
//   「妖怪 対 妖怪」が選ばれない限り、ゲームの動作に何も影響しない。
// - 最初の「作る」「貼る」（`UiOut::open_net`）で別のスレッドがネットワークを開く（`request_open` → `open_game_net`）。
//   ゲームのスレッドは待たず、開く間のメニューに「準備しています」を出す。
//   開き終わったら、ゲームのスレッドが次のtickでネットワークのセッションを`NET`へ組み込み`NET_ON`を立てる（`poll_pending` → `install_slot`）。
// - 再接続: 次が全部揃ったとき、ゲームのスレッドはネットワークの経路のtickの終わりにネットワークのセッションを手放す（`relink_teardown`）: 接続が終わった・`RELINK_GRACE`が経った・場面がマッチの外にある。
//   手放すときにすること: `NET_ON`を0にしてセッションを郵便受け`CLOSING`へ移し、メニューを「接続する前」へ戻すだけ。確保・待ち・スレッドの作成は無い。
//   ワーカーのスレッド（`higanr-relink`）が`relink_tick`でネットワークのグループを後始末する。次の「作る」「貼る」「ルームで待つ」で新しいグループを開く。
//   マッチの中では後始末しない。`offer` / `answer`・観戦・iniの相手の形はメニューを組み込まないので後始末しない。

/// ゲーム内のメニューが動いているかどうか。入力の入口の`route`が見る。
/// 0 → 1にするのは読み取りのスレッド（`arm_game_ui`。メニューのコンテナを設定した後）だけ。
/// 1 → 0にするのはゲームのスレッド（パニック）。
pub static UI_ON: AtomicU32 = AtomicU32::new(0);

struct UiCell(UnsafeCell<Option<NetUi>>);
// SAFETY: 読み取りのスレッドの`arm_game_ui`は`UI_ON`が0の間だけ設定する（起動時の1回。入力の入口の分岐を開く前）。
// 以後はゲームのスレッドだけが`game_ui`を通して触る。
// `UI_ON`はReleaseで1にし、Acquireで読んでから触る。
unsafe impl Sync for UiCell {}
static UI: UiCell = UiCell(UnsafeCell::new(None));

struct LazyNet {
    settings: NetSettings,
    /// 接続コードのフォルダ（開発用の`clipboard = file`のときだけ使う）。ゲームのフォルダの外であることは設定する前に検証した。
    code_dir: Option<PathBuf>,
    /// 起動時に読んだiniのパス。メニューで言語を替えたらこのiniの`[ui] lang`を書き換える。
    ini_path: Option<PathBuf>,
    link: Arc<UiLink>,
}
static LAZY: OnceLock<LazyNet> = OnceLock::new();

impl LazyNet {
    /// ネットワークを開くときの設定: 起動時の設定に、メニューで決定済みのプレイヤー名（`UiLink::player_name`）があればそれを反映したもの。
    /// 名前を変えたら次に開くネットワークから新しい名前で名乗る。開くスレッドだけが呼ぶ。
    fn settings_now(&self) -> NetSettings {
        let mut s = self.settings.clone();
        if let Some(n) = self.link.player_name() {
            s.name = n.as_str().to_string();
        }
        s
    }
}

/// 対戦の後に接続が切れて「妖怪 対 妖怪」へ戻し始めた（ゲームのスレッド）: 知らせの音を別のスレッドが鳴らす。
/// 音の合成はメモリを確保し、鳴らす処理は鳴り終わるまで待つ。そのためゲームのスレッドでは走らせない。
fn play_lost_chime() {
    // 検査のビルドでは鳴らさない。
    #[cfg(not(test))]
    {
        let spawned = std::thread::Builder::new()
            .name("higanr-lost".into())
            .spawn(|| {
                if !crate::notify_win::lost() {
                    game_net_problem(
                        "戻り道: 切れた知らせの音を鳴らせませんでした（PlaySoundW が 0 を返した）。"
                            .to_string(),
                    );
                }
            });
        if let Err(e) = spawned {
            game_net_problem(format!("戻り道: 音を鳴らすスレッドの作成不能（{e}）。"));
        }
    }
}

/// iniの言語の行を書く処理が重ならないためのロック（続けて替えても読み → 書きと一時ファイルを取り合わない）。
static LANG_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// メニューで言語を替えた（ゲームのスレッド）。
fn save_ui_lang(l: Lang) {
    apply_ui_lang(l, spawn_write_ui_lang);
}

/// 今の言語を設定し（アトミック変数1回の書き込み。監視の処理・ウィンドウの文を書くスレッドも読む）、iniへ書く処理を頼む。
/// 頼み方は差し込める（検査は記録するだけの関数を渡す）。
fn apply_ui_lang(l: Lang, ask_write: impl FnOnce()) {
    crate::ui_lang::set(l);
    ask_write();
}

/// iniの`[ui] lang`の書き換えを別のスレッドに頼む（ゲームのスレッドではファイルに触らない。走るのはスレッドを1本作ることだけ）。
/// 書くのは**そのとき設定されている最後の言語**。書き込めない・iniの場所が無いときは本ログへ書く。
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
                game_net_problem(format!("言語: ini へ書けませんでした（{e}）。"));
            }
        });
    if let Err(e) = spawned {
        game_net_problem(format!("言語: 書くスレッドの作成不能（{e}）。"));
    }
}

/// 観戦の「ランダムマッチ」の一覧の接続（メニューが一覧の画面を開いている間だけ持つ）。触るのは`feed_watch_list`だけ。
static WATCH_FEED: std::sync::Mutex<Option<crate::watch_feed::WatchFeed>> =
    std::sync::Mutex::new(None);

/// メニューが観戦の「ランダムマッチ」の一覧の画面に入った（`true`）・出た（`false`）。入るときに一覧の接続のスレッドを作り、出るときに止める
/// （ゲームのスレッド。人が画面を操作したtickに1回。確保・待ちは作ったスレッドの側）。
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

// ---- LAN ----
//
// メニューのLANの項目（「ホスト」「クライアント」「観戦」）を選んだとき、ゲームのスレッドがすることは運び手のスレッド（`lan_link`）を1本作ることだけ（`request_lan`）。
// ホストの受付用受信ソケットと申し込みは運び手が行う。ゲームのネットワークは、参加を受けてから運び手が開く（`lan_open_net` → 開くスレッド → 監視の処理）。
// ゲームのスレッドに足すのは、項目を決定したtickの`request_lan`と、毎tickのアトミック変数1つの読み取り（`lan_watch_poll`）だけ。

/// LANの接続でゲームのネットワークを開く頼み（運び手が出し、開くスレッドが1度だけ取る）。
struct LanOpen {
    /// 対戦用受信ソケットを開く自分のLANのアドレス（ホストは申し込みが届いたアドレス、クライアントは申し込み先へ届くアドレス）。
    ip: Ipv4Addr,
    /// ゲームのネットワークの範囲（`Scope::lan_via`。VPNのアドレスならそのVPNのネットワークを足した範囲）。
    scope: Scope,
    role: LanRole,
    bridge: Arc<LanBridge>,
}
static LAN_OPEN: std::sync::Mutex<Option<LanOpen>> = std::sync::Mutex::new(None);

/// LANの観戦コード（運び手が出し、観戦者のネットワークを開くスレッドが1度だけ取る）と、出した時刻。古いものは使わない。
static LAN_WATCH: std::sync::Mutex<Option<LanWatch>> = std::sync::Mutex::new(None);

struct LanWatch {
    code: String,
    ip: Ipv4Addr,
    scope: Scope,
    at: std::time::Instant,
}
/// 1 = 運び手が試合のホストから観戦コードを受けた（ゲームのスレッドが観戦者のネットワークを開く頼みにする）。
static LAN_WATCH_READY: AtomicU32 = AtomicU32::new(0);
/// 観戦コードを出してから観戦者のネットワークを開くスレッドが取るまでの上限。越えたものは使わない（別の観戦に混ざらないように）。
const LAN_WATCH_FRESH: std::time::Duration = std::time::Duration::from_secs(20);
/// LANの頼みを受けた回数（`request_lan`を呼んだ数。本ログ用）。
static LAN_REQUESTS: AtomicU32 = AtomicU32::new(0);

/// 役割の乱数の最上位のbit。LANのホストは立てて、クライアントは落とす（ホストがいつも試合のホスト = 観戦コードを出す側になる。大きい方がホスト）。
const LAN_HOST_BIT: u64 = 1 << 63;

/// LANの接続の設定: 範囲`scope`（lanか、VPNのネットワークを足したlan）と自分のLANのアドレス（ポートはOSが選ぶ）で開く。
/// STUN・UPnP・IPv6・ルームサーバーは使わない。iniは書き換えない（この接続だけの設定）。
/// `role`があれば役割の乱数を決める。
fn lan_net_settings(
    mut s: NetSettings,
    ip: Ipv4Addr,
    scope: Scope,
    role: Option<LanRole>,
) -> NetSettings {
    s.scope = scope;
    s.bind = Bind::Fixed(std::net::SocketAddr::new(std::net::IpAddr::V4(ip), 0));
    s.stun = None;
    s.stun2 = None;
    s.upnp = false;
    s.ipv6 = false;
    s.room_server = None;
    s.room = None;
    match role {
        Some(LanRole::Host) => s.role_nonce = Some(default_nonce() | LAN_HOST_BIT),
        Some(LanRole::Client) => s.role_nonce = Some(default_nonce() & !LAN_HOST_BIT),
        None => {}
    }
    s
}

#[cfg(test)]
mod lan_tests {
    use super::*;

    #[test]
    fn lan_connections_open_on_the_lan_address_with_no_internet_helpers() {
        let base = crate::net_config::from_text(
            "[netplay]\nenable = 1\nscope = internet\nbind = auto:17723\ncode_side = game\nclipboard = file\nroom_server = https://rooms.example\nstun = stun.example.test:3478\nstun2 = stun.example.test:3479\nupnp = on\nipv6 = on\n",
        )
        .unwrap()
        .unwrap();
        assert!(base.stun.is_some() && base.upnp && base.ipv6 && base.room_server.is_some());
        let ip = Ipv4Addr::new(192, 168, 7, 20);
        let h = lan_net_settings(base.clone(), ip, Scope::Lan, Some(LanRole::Host));
        let c = lan_net_settings(base.clone(), ip, Scope::Lan, Some(LanRole::Client));
        for s in [&h, &c] {
            assert_eq!(s.scope, Scope::Lan);
            assert_eq!(
                s.bind,
                Bind::Fixed(std::net::SocketAddr::new(std::net::IpAddr::V4(ip), 0))
            );
            assert!(s.stun.is_none() && s.stun2.is_none(), "STUN を使う");
            assert!(!s.upnp && !s.ipv6, "UPnP か IPv6 を使う");
            assert!(s.room_server.is_none(), "ルームサーバーを使う");
            // それ以外の設定（名前・形）はそのまま。
            assert_eq!(s.name, base.name);
            assert_eq!(s.mode, base.mode);
        }
        for _ in 0..50 {
            let h = lan_net_settings(base.clone(), ip, Scope::Lan, Some(LanRole::Host));
            let c = lan_net_settings(base.clone(), ip, Scope::Lan, Some(LanRole::Client));
            assert!(
                h.role_nonce.unwrap() > c.role_nonce.unwrap(),
                "ホストが役割の乱数で勝てない"
            );
        }
        // 観戦者の設定は役割の乱数を替えない。
        let w = lan_net_settings(base.clone(), ip, Scope::Lan, None);
        assert_eq!(w.role_nonce, base.role_nonce);
        assert_eq!(w.scope, Scope::Lan);
        // VPNの接続は運び手が決めた範囲（VPNのネットワークを足したlan）で開く。ほかの設定は同じ。
        let vpn_ip = Ipv4Addr::new(26, 12, 34, 56);
        let wide = Scope::LanVpn(higanr_core::vpn_net::Net4::new(vpn_ip, 8).unwrap());
        for role in [Some(LanRole::Host), Some(LanRole::Client), None] {
            let v = lan_net_settings(base.clone(), vpn_ip, wide, role);
            assert_eq!(v.scope, wide);
            assert_eq!(
                v.bind,
                Bind::Fixed(std::net::SocketAddr::new(std::net::IpAddr::V4(vpn_ip), 0))
            );
            assert!(v.stun.is_none() && !v.upnp && !v.ipv6 && v.room_server.is_none());
        }
    }

    // VPNのネットワークは実機の一覧で決まるのでLANの中の値と無いときだけを見る。
    #[test]
    fn the_remembered_lan_address_starts_the_input_only_when_it_is_usable() {
        let at = |s: &str| {
            Some(LanAddr {
                ip: s.parse().unwrap(),
                port: 19000,
            })
        };
        assert!(initial_lan_addr(None).is_empty());
        assert_eq!(
            initial_lan_addr(at("192.168.0.2")).as_str(),
            "192.168.0.2:19000"
        );
        assert_eq!(initial_lan_addr(at("10.9.8.7")).as_str(), "10.9.8.7:19000");
        // このPCのVPNにあるはずのないグローバルアドレスは初期値にしない。
        assert!(initial_lan_addr(at("8.8.8.8")).is_empty());
    }

    #[test]
    fn a_lan_connection_trades_codes_through_the_carrier_and_copies_the_watch_code_for_people() {
        use crate::clipboard::fake::FakeClipboard;
        let human = FakeClipboard::default();
        let h = human.clone();
        let bridge = LanBridge::new();
        let (mut clip, spectate) =
            code_clips(Some(&bridge), move || Ok(Box::new(h) as Box<dyn Clipboard>)).unwrap();
        clip.write("CONNECT-CODE").unwrap();
        assert_eq!(bridge.take_outgoing().as_deref(), Some("CONNECT-CODE"));
        assert!(
            human.writes().is_empty(),
            "接続コードを人のクリップボードへ書いた"
        );
        spectate
            .expect("観戦コードの書き先が無い")
            .write("WATCH-CODE")
            .unwrap();
        assert_eq!(human.writes(), vec!["WATCH-CODE".to_string()]);
        assert!(bridge.take_outgoing().is_none());
        // 人のクリップボードを開けなくてもLANの接続は開く（観戦コードのコピーだけができない）。
        let (_, none) = code_clips(Some(&bridge), || Err("開けない".to_string())).unwrap();
        assert!(none.is_none());
        // LANでない接続: 運び先は人のクリップボード。観戦コードも同じ所へ書く。
        let other = FakeClipboard::default();
        let o = other.clone();
        let (mut clip, spectate) =
            code_clips(None, move || Ok(Box::new(o) as Box<dyn Clipboard>)).unwrap();
        clip.write("CODE").unwrap();
        assert_eq!(other.writes(), vec!["CODE".to_string()]);
        assert!(spectate.is_none());
        assert!(code_clips(None, || Err("開けない".to_string())).is_err());
    }
}

/// 運び手の材料（ゲームのスレッドで作る。確保はあるが、項目が決定されたtickに1回だけ）。
fn lan_env(lazy: &'static LazyNet) -> LanEnv {
    let s = lazy.settings_now();
    // 待ち受けのポート: iniの`bind`のポート（`auto:ポート`か、ポートを書いた固定のアドレス）。無ければ既定。
    let port = match s.bind {
        Bind::Auto(p) => p,
        Bind::Fixed(a) if a.port() != 0 => a.port(),
        Bind::Fixed(_) => DEFAULT_BIND_PORT,
    };
    LanEnv {
        link: Arc::clone(&lazy.link),
        ver: Ver {
            proto: higanr_core::wire::PROTO_VER,
            build: higanr_core::update::BUILD_NUMBER,
        },
        watch_ok: s.spectate.is_some(),
        port,
        reply_wait_ms: higanr_core::lan_proto::HOST_WAIT_REPLY_MS,
        addresses: Box::new(|| {
            adapters_win::list().map(|l| higanr_core::adapter_pick::host_addresses(&l))
        }),
        vpn_links: Box::new(|| {
            adapters_win::list()
                .map(|l| higanr_core::adapter_pick::vpn_links(&l))
                .unwrap_or_default()
        }),
        net_free: Box::new(|| OPEN_STATE.load(Ordering::SeqCst) == OPEN_IDLE),
        open_net: Box::new(lan_open_net),
        start_watch: Box::new(lan_start_watch),
        note: Box::new(lan_note),
        id: default_nonce() as u32,
        unix: net_udp::now_unix,
        stop: Arc::new(core::sync::atomic::AtomicBool::new(false)),
    }
}

/// 運び手の行（本ログと「ネットワーク」の詳細へ書く）。
fn lan_note(line: String) {
    crate::main_log::push(line.clone());
    game_net_note(line);
}

/// 運び手の頼み: ゲームのネットワークをLANの設定で開く（開くスレッドを作って返る。開き終わるとゲームのスレッドが次のtickで組み込む）。
fn lan_open_net(
    ip: Ipv4Addr,
    scope: Scope,
    role: LanRole,
    bridge: Arc<LanBridge>,
) -> Result<(), String> {
    let lazy = LAZY
        .get()
        .ok_or("ゲーム内のメニューが置かれていない".to_string())?;
    if OPEN_STATE
        .compare_exchange(OPEN_IDLE, OPEN_BUSY, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("ネットワークがほかの用で使われている".to_string());
    }
    SPECTATE_AFTER.store(0, Ordering::SeqCst);
    *LAN_OPEN.lock().unwrap_or_else(|e| e.into_inner()) = Some(LanOpen {
        ip,
        scope,
        role,
        bridge,
    });
    spawn_open(lazy, false);
    Ok(())
}

/// 運び手の頼み: 試合のホストから観戦コードを受けた。保存して、ゲームのスレッドに観戦者のネットワークを開かせる（`lan_watch_poll`）。
fn lan_start_watch(code: String, ip: Ipv4Addr, scope: Scope) -> Result<(), String> {
    *LAN_WATCH.lock().unwrap_or_else(|e| e.into_inner()) = Some(LanWatch {
        code,
        ip,
        scope,
        at: std::time::Instant::now(),
    });
    LAN_WATCH_READY.store(1, Ordering::SeqCst);
    Ok(())
}

/// ゲームのスレッド（毎tick。原子変数1つの読み）: 運び手が観戦コードを受けたなら、観戦者のネットワークを開く頼みを出す。
fn lan_watch_poll() {
    if LAN_WATCH_READY.load(Ordering::Relaxed) != 0
        && LAN_WATCH_READY.swap(0, Ordering::SeqCst) != 0
    {
        request_spectate();
    }
}

fn lan_done(end: lan_link::End) {
    game_net_note(format!("LAN: 運び手の務めを終えた（{end:?}）"));
}

/// メニューのLANの項目が決定された（ゲームのスレッド）。運び手（LANの接続コードを運ぶスレッド）を作る。
/// 待っているだけのセッションが残っていれば手放す（対戦のホストの受信ポートを空けるため）。
/// 運び手は後始末が済むのを待ってから受信ポートを開く。動いている運び手があれば読み捨てる。
fn request_lan(ask: LanAsk) {
    LAN_REQUESTS.fetch_add(1, Ordering::Relaxed);
    let Some(lazy) = LAZY.get() else {
        return;
    };
    if lan_link::is_active() {
        game_net_note("LAN: 動いている運び手があるので、頼みを読み捨てた".to_string());
        return;
    }
    if OPEN_STATE.load(Ordering::SeqCst) == OPEN_DONE {
        release_slot(|s| match s {
            Slot::Play(p) => matches!(p.np.ui_conn(0).0, Conn::Waiting),
            Slot::Watch(w) => w.wp.is_ended() && spectate_join::finished(),
        });
    }
    let env = lan_env(lazy);
    let spawned = match ask {
        LanAsk::Host => lan_link::spawn("higanr-lan", env, lan_link::run_host, lan_done),
        LanAsk::Client(a) => lan_link::spawn(
            "higanr-lan",
            env,
            move |e| lan_link::run_client(e, a),
            lan_done,
        ),
        LanAsk::Watch(a) => lan_link::spawn(
            "higanr-lan",
            env,
            move |e| lan_link::run_watch(e, a),
            lan_done,
        ),
    };
    match spawned {
        Ok(true) => {}
        Ok(false) => game_net_note("LAN: 動いている運び手があるので、頼みを読み捨てた".to_string()),
        Err(e) => {
            game_net_problem(e);
            lazy.link.publish(&Note {
                event: Event::Internal,
                seq: lazy.link.note().map_or(1, |n| n.seq.wrapping_add(1)),
                ..Note::default()
            });
        }
    }
}

/// ネットワークの開き状態:
/// - 0: まだ開いていない（開けなかった・後始末を終えたのでやり直せる場合も含む）。
/// - 1: 別のスレッドが開いている最中。
/// - 2: ネットワークのセッションを組み込んだ。
/// - 3: 接続が終わってゲームのスレッドがネットワークのセッションを手放し、ワーカーのスレッドが後始末している最中。
/// - 4: 後始末している最中に人が「作る」「貼る」「ルームで待つ」を選んだ（再接続のスレッドが後始末を終えたらそのまま開く）。
static OPEN_STATE: AtomicU32 = AtomicU32::new(0);
const OPEN_IDLE: u32 = 0;
const OPEN_BUSY: u32 = 1;
const OPEN_DONE: u32 = 2;
const OPEN_CLOSING: u32 = 3;
const OPEN_REOPEN: u32 = 4;

/// 開くスレッドが作ったネットワークのセッション（ゲームのスレッドが次のtickで取って組み込む）。対戦のセッションか、観戦者のセッション（「ルームで観戦」）。
enum Pending {
    Play(Box<NetSlot>),
    Watch(Box<WatchNetSlot>),
}
// SAFETY: 開くスレッドが作ってここへ**渡す**（移す）だけ。渡した後は開くスレッドは触らない。取り出して使うのはゲームのスレッドだけ。
// ネットワークのセッションの中身（`NetPlay`・`WatchPlay`・`GameNet`）はゲームのスレッドから使われる前提で作ってある。
// 作ったスレッドに縛られる物（スレッドのローカル・`Rc`）は持たない（`open_net`は読み取りのスレッドで作って、ゲームのスレッドへ渡している）。
unsafe impl Send for Pending {}
static PENDING: std::sync::Mutex<Option<Result<Pending, ()>>> = std::sync::Mutex::new(None);
/// ゲームのスレッドがネットワークのセッションを組み込めなかった回数（準備が壊れているときだけ。理由の文はゲームのスレッドでは作らず、数だけ本ログへ出す）。
static INSTALL_FAILS: AtomicU32 = AtomicU32::new(0);
/// ネットワークを開く仕事を始めた回数（開くスレッドが数える。試験のまとめが「頼みが来たか」を見る）。
static OPEN_TRIES: AtomicU32 = AtomicU32::new(0);
/// ネットワークを開く仕事の本ログの文（`net_status`が「ネットワークの外」の行へ足す）。
static GAME_NET_NOTES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// 失敗の文。「ネットワークの外」の行（詳細）に加えて、簡潔なログにもその場で1行出す。
fn game_net_problem(line: String) {
    crate::main_log::push(line.clone());
    game_net_note(line);
}

/// ネットワークを開いた行。観戦の準備の失敗の句が混ざっていれば簡潔にも出す（成功の文は詳細だけ）。
fn game_net_open_note(line: String) {
    if crate::spectate_host::note_has_problem(&line) {
        game_net_problem(line);
    } else {
        game_net_note(line);
    }
}

fn game_net_note(line: String) {
    if let Ok(mut g) = GAME_NET_NOTES.lock() {
        if g.len() >= 32 {
            g.remove(0);
        }
        g.push(line);
    }
}

// ---- 再接続 ----

enum Released {
    Play(Box<NetSlot>),
    Watch(Box<WatchNetSlot>),
}

struct Closing {
    slot: Released,
    link: Arc<UiLink>,
    /// 手放したtickまでに送られた指示の数（`UiLink::order_mark`）。これより前の指示は前の監視の処理の残りとして捨てる。
    mark: u32,
}
// SAFETY: ゲームのスレッドは移した後に触らない（`NET`からは取り出し済み・`NET_ON`は0）。
// 取り出して落とすのはワーカーのスレッドだけ。中身は`Pending`と同じく作ったスレッドに縛られる物を持たない。
unsafe impl Send for Closing {}
/// 手放したセッションを後始末のスレッドへ渡す郵便受け（ゲームのスレッドは`try_lock`だけ。取れなければ次のtickに手放す）。
static CLOSING: std::sync::Mutex<Option<Closing>> = std::sync::Mutex::new(None);
/// 1は、手放したネットワークのセッションを後始末したら続けて観戦者のネットワークを開くことを表す。
/// ゲームのスレッドが書き、ワーカーのスレッドが読んで0に戻す。
/// 「作る」「貼る」「ルームで待つ」が後から選ばれたら、ゲームのスレッドが0に戻す（最後に選んだ方が勝つ）。
static SPECTATE_AFTER: AtomicU32 = AtomicU32::new(0);
/// ワーカーのスレッドを作ったかどうか（1は作った。0の実行では手放さない。後始末する者が居ないので、今までどおりネットワークは残る）。
static RELINK_STARTED: AtomicU32 = AtomicU32::new(0);
/// ネットワークが終わってから手放すまでの待ち（ms。`netplay::RELINK_GRACE_MS`。検査は短くする）。
static RELINK_GRACE: AtomicU32 = AtomicU32::new(netplay::RELINK_GRACE_MS);
/// ワーカーのスレッドが郵便受けを見に行く間隔。後始末の後に「作る」が開き始めるまでの遅れの上限になる。
/// マッチの外での人の操作なので、この遅れには気づかない長さにしてある。
const RELINK_POLL: std::time::Duration = std::time::Duration::from_millis(20);
static LINKS_CLOSED: AtomicU32 = AtomicU32::new(0);
static PAST_LINKS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
/// 本ログに残す後始末した接続の数の上限（古い方から捨てる。1つ数KB）。
const PAST_LINKS_MAX: usize = 16;
/// 後始末した観戦者のセッションの数とその節（本ログの「ネットワークの外」の終わりに足す。観戦者の集計`WATCH_STATUS`は次の観戦で上書きされる）。
static WATCHES_CLOSED: AtomicU32 = AtomicU32::new(0);
static PAST_WATCHES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
/// 後始末した観戦の節のうち、まだ本ログの途中に書いていないもの（`take_watch_ends`が取り出して空にする）。
/// 観戦者はゲーム内のメニューの実行で計測が締まらない。そのため後始末した次の1秒のポーリングで読み取りのスレッドが書く。
/// 上限は`PAST_LINKS_MAX`。
static UNLOGGED_WATCHES: std::sync::Mutex<Vec<(u32, String)>> = std::sync::Mutex::new(Vec::new());
/// 今`CODE_WATCH`に残っている監視の処理の文は後始末したときに`PAST_LINKS`へコピー済み（二重に出さない）。
static CODE_NOTES_ARCHIVED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// 接続した（ことがある）接続のsession（ゲームのスレッドが手放すときに書き、開くスレッドが`GameUi::spent`へコピーする）。
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

/// ネットワークのセッションを手放す（ゲームのスレッド）。対戦のセッションだけが対象。
/// `route`がネットワークの経路のtickの後に`NetPlay::relink_due`が真のときだけ呼ぶ。
/// 確保しない（セッションは`Box`のまま郵便受けへ移す）・待たない（`try_lock`。取れなければ次のtick）・スレッドを作らない。
/// 手放した後は次のtickからメニューだけの経路（`ui_only_tick`）になる。
fn relink_teardown() {
    release_slot(|s| matches!(s, Slot::Play(_)));
}

/// 観戦者のセッションを手放す（ゲームのスレッド）。呼ぶのは、観戦者のセッションが終わり（`Ended`）、観戦者のルームのスレッドが最後の状態を書き終えたとき。
/// メニューは元の項目に戻る。
fn watch_teardown() {
    release_slot(|s| matches!(s, Slot::Watch(w) if w.wp.is_ended() && spectate_join::finished()));
}

/// ネットワークのセッション（対戦・観戦）を手放す共通の部分（ゲームのスレッド）。`allow`がセッションの状態を見て真のときだけセッションを郵便受けへ移す。
/// 手放せたらtrue。確保しない・待たない（`relink_teardown`の規律）。
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
    // SAFETY: ゲームのスレッド（`route`の中。ネットワークのセッションの借用は終わっている）。ネットワークのセッションに触るのはゲームのスレッドだけ（`NetCell`のSyncの注）。
    let cell = unsafe { &mut *NET.0.get() };
    if !cell.as_ref().is_some_and(allow) {
        return false;
    }
    // 先にネットワークの経路を閉じる（開始のフック・rollbackのフックは`NET_ON`を見てセッションに触らない。どれも同じゲームのスレッド）。
    NET_ON.store(0, Ordering::SeqCst);
    let released = match cell.take() {
        Some(Slot::Play(slot)) => {
            if let Some(s) = slot.np.spent_session() {
                spend(s);
            }
            // 後始末する直前の接続の状態（終わりの状態なので成立の前の残りの時計は使わない）。
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

/// ワーカーのスレッドの1回分の処理（`RELINK_POLL`ごと）。後始末している間に人が選んでいたら最初のネットワークと同じ処理（`open_game_net`）で開く。
fn relink_tick() {
    relink_tick_with(|| {
        // 「ルームで観戦」が決定されて手放した（か、後始末している間に選ばれた）なら観戦者のネットワークを開く。そうでなければ対戦のネットワークを開く。
        // 予定は開く前に消す（開けなくても次の後始末へ持ち越さない）。
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

/// `relink_tick`の中身（検査は開く処理を偽物にして直接呼ぶ）。手放されたネットワークのセッションがあれば前の接続のネットワークの行を書き留めてからネットワークのグループを後始末する。
/// 後始末している間に人が選んでいた（`OPEN_REOPEN`）なら、`reopen`で新しいグループを開いてゲームのスレッドへ渡す（`PENDING`）。
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
    // 手放した後に開くもの: 「ルームで観戦」が決定済みだったか、後始末している間に何かが選ばれていた（`OPEN_REOPEN`）ときは開く。
    // 何も選ばれていなければ開き直せる状態（`OPEN_IDLE`）に戻す。
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

/// 手放した対戦のセッションを後始末する（ワーカーのスレッド）。
/// 前の接続のネットワークの行を本ログ用に控えてから、ネットワークのグループ（監視の処理・受信スレッド・受信ソケット）を止めて閉じる。
fn close_play(slot: NetSlot, link: &Arc<UiLink>, mark: u32) {
    let n = LINKS_CLOSED.fetch_add(1, Ordering::SeqCst) + 1;
    // ゲームのスレッドはもうネットワークのカウンタをコピーしない（`NET_ON`が0）のでここで読む値が前の接続の最後の値。
    let mut text = format!(
        "\n==== {n} 回目の接続（片づけ済み。この節の数はこの接続のぶん）====\nネットワーク: {}\n{}\n{}{}",
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
                let (stats, spectate) = net_code_game::close_parts(np, net, w, r, link, mark);
                text += &format!("\n受信 {stats:?}");
                text += &closed_spectate_text(spectate.as_ref());
                for l in w.notes() {
                    text += &format!("\n{l}");
                }
            }
            (w, r) => {
                // 来ない見込み（ゲーム内のメニューの実行は開くときに監視の処理と受信スレッドを必ず作る）。在るものだけ止める。
                drop((np, net));
                if let Some(w) = w {
                    w.stop();
                }
                let spectate = r.and_then(|r| r.stop_with_spectate().1);
                link.reset_for_next_watcher(mark);
                text += "\n（片づけ: 見張りか受信スレッドが無かった）";
                text += &closed_spectate_text(spectate.as_ref());
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
        "ネットワーク: {n} 回目の接続を片づけました（見張りと受信の処理を停止し、受信ソケットを閉じました。メニューは「接続前」）。"
    ));
}

/// 止めた時点の観戦のホストの集計（観戦・観戦者のツリー・上り回線速度の測定の行）。後始末した対戦の節に足す。
/// 受信スレッドが無くなると`net_status`はこれらを読めない。そのため止めた直後に控える。観戦のホストを使っていなければ空。
fn closed_spectate_text(spec: Option<&spectate_host::Snapshot>) -> String {
    spec.map(|s| format!("\n{}", s.lines(crate::log_mode::detail())))
        .unwrap_or_default()
}

/// 手放した観戦者のセッションを後始末する（ワーカーのスレッド）。
/// 観戦者のルームのスレッドと受信スレッドを止めて受信ソケットを閉じ、観戦者の集計の行を書き留める。
/// 指示のリングバッファに残った指示は捨てる。状態は消さない（観戦の終わりの理由を左上に残す）。
/// 節には観戦者のルームのカウンタの行も入れる。読み取りのスレッドが次の1秒のポーリングで本ログの途中に書く（`take_watch_ends`）。
fn close_watch(w: WatchNetSlot, link: &Arc<UiLink>, mark: u32) {
    let WatchNetSlot { wp, net, .. } = w;
    // 最後のカウンタを書き留める（セッションを落とす前に。`WATCH_STATUS`はゲームのスレッドが最後にコピーした値）。
    let watch_text = WATCH_STATUS.text();
    drop(wp);
    drop(net);
    // ルームのカウンタの行はこの節にだけ入れる（`spectate_join::stats_text`は動いている分だけ。書き留める場所が2つあると全文で重なる）。
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
        "観戦: {n} 回目の観戦者のセッションを片づけました（ルームと受信の処理を停止し、受信ソケットを閉じました。メニューは「接続前」）。"
    ));
}

/// 後始末した観戦の節の中身（ヘッダは呼び出し側が付ける）: ルームのカウンタの行・観戦者の集計（無ければ飛ばす）・受信のカウンタ。
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

/// 後始末した観戦の節のうち、まだ本ログの途中に書いていないものを取り出す（読み取りのスレッド）。
/// 取り出したら空になるので1つの節は1回だけ書かれる。番号は`close_watch`の「n 回目の観戦」。
pub fn take_watch_ends() -> Vec<(u32, String)> {
    std::mem::take(&mut *UNLOGGED_WATCHES.lock().unwrap_or_else(|e| e.into_inner()))
}

/// 観戦者のセッションがある間（ルームのスレッドが動いている・観戦中）の本ログの途中経過の文（読み取りのスレッド）: いまのルームのカウンタの行と観戦者の集計（あれば）。
/// 観戦者のセッションが無い実行は`None`。
pub fn watch_live_text() -> Option<String> {
    // 観戦者の実行の1秒ごとの読み。マッチの終わりの選択のイベントを本ログへ渡す。
    crate::end_outline_win::flush_notes();
    let join = spectate_join::active_line()?;
    Some(if WATCH_STATUS.published() {
        format!("{join}\n{}", WATCH_STATUS.text())
    } else {
        join
    })
}

/// ワーカーのスレッドを作る（開くスレッドから。実行で1回だけ）。
/// 作れなければ手放さない（ネットワークは接続が終わっても残り、起動し直すまで開き直せない）。
fn start_relink_thread() {
    if RELINK_STARTED.swap(1, Ordering::SeqCst) != 0 {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("higanr-relink".into())
        .spawn(|| {
            loop {
                std::thread::sleep(RELINK_POLL);
                // パニックは受けて外へ出さない（スレッドの入口）。後始末の途中でパニックしたら開き直せる状態に戻す。
                if catch_unwind(relink_tick).is_err() {
                    game_net_problem(
                        "ネットワーク: 片づけのワーカーでパニックしました（開き直せる形に戻した）。"
                            .to_string(),
                    );
                    OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
                }
            }
        });
    if let Err(e) = spawned {
        RELINK_STARTED.store(0, Ordering::SeqCst);
        game_net_problem(format!(
            "ネットワーク: 片づけのワーカーのスレッドの作成不能（{e}）。接続が終わった後は起動し直すまで開き直せません。"
        ));
    }
}

/// ゲーム内のメニューを組み込む（読み取りのスレッド。`SCRIPT_ON`と`NET_ON`が0の間だけ）。ネットワークは開かない。
/// メニューのセッションとネットワークを開くのに要るものを用意し、`arm_net`と同じ準備のチェックをする。
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
        higanr_core::update_notice::allowed_by_env(
            std::env::var_os(higanr_core::update_notice::NOTICE_ENV).as_deref(),
        ),
    )
}

/// 新しいバージョンの知らせを許した配布ビルド（`build_number`が0でない）だけ、確認の専用のスレッドを作って頼みの窓口を返す。
/// 許さない起動では`None`を返す（ゲームのスレッドは頼みも読みもしない）。
fn update_hooks(notice: bool, build_number: u32) -> Option<crate::net_ui::UpdateHooks> {
    if !notice || build_number == 0 {
        return None;
    }
    crate::update_check::start();
    Some(crate::net_ui::UpdateHooks {
        ask: crate::update_check::ask,
        found: crate::update_check::found,
    })
}

/// LANのアドレスの入力の初期値（iniの`lan_host`）。LANの外のアドレスは、起動時に自分のPCのVPNのネットワークの中にあるときだけ使う。
/// アダプターの一覧を取得するのは、LANの外のアドレスが書かれているときだけ。
fn initial_lan_addr(host: Option<LanAddr>) -> AddrBuf {
    let Some(b) = host.map(LanAddr::to_buf) else {
        return AddrBuf::new();
    };
    if b.parse().is_ok() || b.parse_with(&crate::vpn_snap::scan()).is_ok() {
        b
    } else {
        AddrBuf::new()
    }
}

/// `arm_game_ui`の中身（メニューが始まる言語と、読み取り可能かをチェックする範囲を渡す。検査は自分の言語と静的変数を渡す）。
/// `notice`はローダーが新しいバージョンの知らせを許したかどうか（`update_notice::NOTICE_ENV`）。
/// 許したときだけ確認の専用のスレッドをここで作り、ゲームのスレッドの確認の頼みを渡す。
fn arm_game_ui_with(
    s: &NetSettings,
    set_state_fn: u32,
    code_dir: Option<PathBuf>,
    ini_path: Option<PathBuf>,
    lang: Lang,
    ranges: &[(u32, u32)],
    notice: bool,
) -> Result<String, String> {
    if SCRIPT_ON.load(Ordering::SeqCst) != 0 || NET_ON.load(Ordering::SeqCst) != 0 {
        return Err("入口の分岐が既に開いています".to_string());
    }
    check_ready(set_state_fn, ranges)?;
    if UI_ON.load(Ordering::SeqCst) != 0 || LAZY.get().is_some() {
        return Err("ゲーム内のメニューは設定済み".to_string());
    }
    SET_STATE_FN.store(set_state_fn, Ordering::SeqCst);
    GAME_TID.store(0, Ordering::SeqCst);
    let link = Arc::new(UiLink::new());
    let (ini, start_auto) = menu_start(s);
    // ルーム名の入力の初期値: iniの`room`（決まりを通るもの）→ 空。メニューで決定した名は`room`に書き戻してあるので次の起動で同じ値から始まる。
    let (initial, from) =
        higanr_core::room_input::initial_from(s.room.as_ref().map(|r| r.0.as_str()));
    crate::ini_save::set_target(ini_path.clone());
    LAZY.set(LazyNet {
        settings: s.clone(),
        code_dir,
        ini_path,
        link: Arc::clone(&link),
    })
    .map_err(|_| "ゲーム内のメニューは設定済み".to_string())?;
    let room = room_item(s, initial);
    // 「名前」の項目: 今の名前（打てない文字は`?`に直した表示用の形）。
    // メニューで決定した名前・ルーム名・同期方式・ディレイ・サイドは、決定したときにゲームのスレッドが作った別のスレッドがiniの該当の行へ書く（`ini_save`）。
    // 「サイド」の初期値は「おまかせ」（iniに`host_seat`があればそのサイド）。
    let ui = NetUi::new(ini, link, &BOARD)
        .with_start_auto(start_auto)
        .with_room(room)
        .with_seat(initial_seat(s))
        .with_player_name(NameBuf::lossy(&s.name), Some(crate::ini_save::save_name))
        .with_room_saver(Some(crate::ini_save::save_room))
        .with_lan_addr(
            initial_lan_addr(s.lan_host),
            Some(crate::ini_save::save_lan_host),
        )
        .with_vpn_hooks(Some(crate::net_ui::VpnHooks {
            scan: crate::vpn_snap::request_scan,
            read: crate::vpn_snap::read,
        }))
        .with_shape_saver(Some(crate::ini_save::save_shape))
        .with_seat_saver(Some(crate::ini_save::save_seat))
        .with_watch_feed(Some(feed_watch_list))
        .with_update_check(
            update_hooks(notice, higanr_core::update::BUILD_NUMBER),
            higanr_core::update::BUILD_NUMBER,
        )
        .with_ini_notice(higanr_core::net_menu::IniNotice::from_notes(&s.ini_notes))
        .with_spectate_hint(s.spectate.is_some())
        .with_lang(lang, Some(save_ui_lang))
        .with_status_side(s.status_side)
        .with_split_keyboards(s.split_keyboards)
        .with_return_flow(Some(play_lost_chime))
        .with_return_log(Some(publish_return_stats));
    // SAFETY: `UI_ON`が0なのでゲームのスレッドはメニューのコンテナに触らない（`UiCell`のSyncの注）。
    unsafe { *UI.0.get() = Some(ui) };
    UI_ON.store(1, Ordering::Release);
    Ok(format!(
        "ゲーム内のメニューを置いた（受信ソケット・STUN・見張りは、メニューで最初に「{}」「{}」「{}」「{}」のどれかを選ぶまで開かない。コードの受け渡しは{}。「{}」は{}）",
        higanr_core::net_menu::T_RANDOM.ja,
        higanr_core::net_menu::T_ROOM.ja,
        higanr_core::net_menu::T_CREATE.ja,
        higanr_core::net_menu::T_PASTE.ja,
        match s.clipboard {
            ClipboardKind::System => "クリップボード",
            ClipboardKind::File => "ファイル（開発用 clipboard = file）",
        },
        higanr_core::net_menu::T_ROOM.ja,
        match (room, from) {
            (RoomItem::Off, _) => "出さない（room_server が無い）",
            // 名は書かない。初期値の出どころだけ。
            (RoomItem::On(_), InitialFrom::Ini) => {
                "出す（ルーム名はメニューで打つ。初めの値は ini の room。名は記録しない）"
            }
            (RoomItem::On(_), InitialFrom::Empty) => {
                "出す（ルーム名はメニューで打つ。初めの値は空。名は記録しない）"
            }
        }
    ))
}

/// 「ルームマッチ」の項目（iniの`room_server`が無ければ出さない。あれば入力の初期値`initial`を持たせる）。
pub(crate) fn room_item(s: &NetSettings, initial: higanr_core::room_input::RoomBuf) -> RoomItem {
    match &s.room_server {
        None => RoomItem::Off,
        Some(_) => RoomItem::On(initial),
    }
}

/// メニューの「同期方式」「ディレイ」の初期値（iniの形式と、ディレイがautoかどうか）。
/// ディレイはiniの`delay`から取る（autoか、どちらの形式でも同じ数）。
/// autoのときのDはrollbackの既定（メニューでrollbackへ替えてautoのままならAGREEのDになる）。
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

/// メニューの「サイド」の初期値: iniに`host_seat`を書いてあればそのサイド。無ければ「おまかせ」。
pub(crate) fn initial_seat(s: &NetSettings) -> higanr_core::wire::SeatWish {
    if s.host_seat_given {
        higanr_core::seat_pick::SeatAsk::plain(s.host_seat).wish
    } else {
        higanr_core::wire::SeatWish::Any
    }
}

/// メニューのカウンタ（`UiStats`のコピー。ゲームのスレッドが書き、読み取りのスレッドが本ログへ読む）。
static UI_STATS: [AtomicU32; 7] = [const { AtomicU32::new(0) }; 7];

/// 戻り道のカウンタ（始めた・着いた・途中でやめた・最後にやめた理由のコード）。変わるたびにゲームのスレッドが書く（アトミック変数4つ。確保しない）。
static RET_STATS: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

fn publish_return_stats(st: &net_ui::UiStats) {
    for (a, v) in RET_STATS.iter().zip([
        st.returns,
        st.return_arrived,
        st.return_gave_up,
        st.return_why,
    ]) {
        a.store(v, Ordering::Relaxed);
    }
}

/// 本ログの「戻り道」の行。始めなかった回も0回と書く（0回を「測れていない」と区別するため）。
fn return_stats_text() -> String {
    let l = |i: usize| RET_STATS[i].load(Ordering::Relaxed);
    format!(
        "\n戻り道: 始めた {} 回 / 着いた {} 回 / 途中でやめた {} 回（最後の理由: {}）",
        l(0),
        l(1),
        l(2),
        higanr_core::return_flow::GiveUp::describe(l(3))
    )
}

/// 本ログの「ネットワークの外」の行へ足すメニューのカウンタ（メニューが無い実行は空）。
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
    ) + &return_stats_text()
        + &format!("\n{}", crate::split_kb_win::status_text())
}

/// 台本の`type=` / `bs=` / `enter=`の文字を、ルーム名の入力欄へ入れた・入力欄が閉じていて捨てた単位の数（ゲームのスレッドが書き、本ログが読む。
/// 実機の検証で台本の文字が届いたかを見る）。
static TYPED_PUSHED: AtomicU32 = AtomicU32::new(0);
static TYPED_DROPPED: AtomicU32 = AtomicU32::new(0);

/// ゲーム内のメニューが組み込まれていて、ネットワークをまだ開いていないかどうか（計測が「ネットワークが終わるのを待つ」判定で使う）。
pub fn net_pending() -> bool {
    UI_ON.load(Ordering::SeqCst) != 0 && NET_ARMED.load(Ordering::SeqCst) == 0
}

/// ネットワークを開く頼み（ゲームのスレッド）。別のスレッドに開かせる（受信ソケット・STUNの問い合わせは待つため）。
/// すでに開いている最中・開いた後は何もしない。
/// ゲームのスレッドで走るのは、人が「作る」「貼る」を選んだtickに1回スレッドを1本作ることだけ。
/// そのtickはマッチもlockstepも動いていない（ネットワークのセッションがまだ無い）ので遅延に効かない。
/// 起動時に待たせておく案は「選ばれるまで何も起こさない」に反するので採らない。
fn request_open() {
    // 前の接続を再接続のスレッドが後始末している最中（`OPEN_CLOSING`）なら後始末を終えたスレッドがそのまま開く（`OPEN_REOPEN`。ここではスレッドを作らない）。
    // 後始末を終えた瞬間と重なったらもう一度見て`OPEN_IDLE`からの経路へ行く（3回まで）。
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
                // 後始末の後に観戦者のネットワークを開く予定だったなら最後に選んだ「作る」「貼る」「ルームで待つ」が勝つ。
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
    // 後始末の最中に観戦の予定だけが残った（後始末を終えた瞬間と重なった）なら消す（次の後始末で観戦者のネットワークを開いてしまう）。
    SPECTATE_AFTER.store(0, Ordering::SeqCst);
    let Some(lazy) = LAZY.get() else {
        OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
        return;
    };
    spawn_open(lazy, false);
}

/// ネットワーク（対戦のネットワークか観戦者のネットワーク）を開くスレッドを作る（ゲームのスレッド。`OPEN_BUSY`に変えた後に1回だけ呼ぶ）。
/// 作れなければ本ログに書き、メニューへ「内部の不具合」を知らせて、やり直せる状態（`OPEN_IDLE`）に戻す。
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
        game_net_problem(format!("ネットワーク: 開くスレッドの作成不能（{e}）。"));
        let n = Note {
            event: Event::Internal,
            seq: 1,
            ..Note::default()
        };
        lazy.link.publish(&n);
        OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
    }
}

/// 「ルームで観戦」が決定された（ゲームのスレッド）。**対戦のネットワークは開かない。** 状態ごとの扱い:
/// - `OPEN_IDLE`: 開くスレッドを作る。`OPEN_CLOSING`: 後始末を終えたワーカーのスレッドが続けて開く（`SPECTATE_AFTER`）。
/// - `OPEN_DONE`: 接続していないセッション（待っているだけの対戦のセッション・終わった観戦者のセッション）は手放して続けて開く。
///   使っているセッションは手放さず、指示を読み捨てる（来ない見込み）。
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
                    // 後始末を終えた瞬間と重なった（`OPEN_CLOSING`の枝で予定を書いた後に`OPEN_IDLE`へ戻された）なら予定は要らない（ここで直接開く）。
                    // 残すと次の後始末で2本目を開く。
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
                    // 対戦のセッション: まだ誰とも接続していない（待っているだけ。メニューが「ルームで観戦」を出すのはこのときだけ）。
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
/// 「ルームで観戦」の頼みを受けた回数（`request_spectate`を呼んだ数。本ログ用）。
static SPECTATE_REQUESTS: AtomicU32 = AtomicU32::new(0);

/// 検査だけ: 1にすると次に`UiOut`を見る場所（`spectate_asked`）が「ルームで観戦が決定された」と読んで0に戻す。
/// ウィンドウから文字を打てない検査から観戦の決定を作るために使う。
#[cfg(test)]
static FORCE_SPECTATE_OPEN: AtomicU32 = AtomicU32::new(0);

/// このtickのメニューの結果が「ルームで観戦」の決定かどうか。
fn spectate_asked(out: &UiOut) -> bool {
    #[cfg(test)]
    if FORCE_SPECTATE_OPEN.swap(0, Ordering::SeqCst) != 0 {
        return true;
    }
    out.spectate_open
}

/// 起動時にrollbackの入口を書くかを決める「望み」。**対戦は望みによらず`Rollback`**（iniがdelay方式でもキャラセレクトのTabでrollbackへ替えられるように）。
/// 観戦者はマッチを動かさないので、iniがdelay方式の実行は1バイトも書かない（走査のcallだけを`arm_watch_scan`が設定する）。
fn install_wish(s: &NetSettings) -> higanr_core::wire::Mode {
    if s.code_side == Some(CodeSide::Spectate) {
        s.mode_wish().mode
    } else {
        higanr_core::wire::Mode::Rollback
    }
}

/// マッチの経路へ渡す`netplay::Config`（`open_net`と`open_game_net`が同じ値で作る）。
fn play_config(s: &NetSettings, rb_wired: bool) -> netplay::Config {
    netplay::Config {
        check: s.check,
        wait_budget_us: s.wait_budget_us,
        lat_plan: s.lat_plan,
        // ネットワークの選択を通らない場面2（タイトルのデモ・リプレイの再生など）では止める地点に入らない（`netplay::Config::require_net_selection`）。
        require_net_selection: true,
        // HELLOで名乗る望むモード。iniがrollbackでもrollbackのマッチの経路の入口を全部書けていない実行はdelay方式を名乗る。
        // 書けなかったのにrollbackを名乗ると、相手は予測で進めるのにこちらは待つことになる。
        mode_wish: s.mode_wish_if(rb_wired),
        rollback_ahead: s.rollback_ahead,
        // rollbackが「できる」かどうか（HELLOの +3のbit1。望みとは別）。
        // 入口を全部書けていれば、両者がrollbackできるペアはdelay方式を望んでいてもキャラセレクトのTabでrollbackへ替えられる。
        rollback_able: rb_wired,
        // 対戦の成立をREADYで待つのはゲーム内のメニューの実行だけ（両者が「妖怪 対 妖怪」を選んだら成立。`match_gate`）。
        // `offer` / `answer`は接続したときに成立する。
        match_gate: s.code_side == Some(CodeSide::Game),
        // 遅れの量をホストが決める（`delay_pick`）のもゲーム内のメニューの実行だけ。iniの`lat_plan`を書いた実行は予定のほうを使う。
        delay_pick: s.code_side == Some(CodeSide::Game) && s.lat_plan.is_empty(),
        // フレームの始まりで相手の入力ワードを待つ（iniの`head_wait`。既定1）。
        head_wait_cap_us: if s.head_wait {
            netplay::HEAD_WAIT_CAP_US
        } else {
            0
        },
        // ホストの決め方はゲーム内のメニューの監視の処理が接続ごとに決める（`LinkSlot::set_host_pick`）。
        host_pick: None,
    }
}

/// 人とゲームの間でコードを受け渡すクリップボード（iniの`clipboard`。開発用の`file`はコードのフォルダの下のファイル）。
fn open_clipboard(s: &NetSettings, lazy: &LazyNet) -> Result<Box<dyn Clipboard>, String> {
    Ok(match (s.clipboard, lazy.code_dir.as_deref()) {
        (ClipboardKind::System, _) => Box::new(SystemClipboard),
        (ClipboardKind::File, Some(d)) => {
            std::fs::create_dir_all(d)
                .map_err(|e| format!("場所 {} の作成不能: {e}", d.display()))?;
            Box::new(FileClipboard::new(d.to_path_buf()))
        }
        (ClipboardKind::File, None) => return Err("接続コードの場所が未決定".to_string()),
    })
}

/// 監視の処理のコードの運び先（`GameUi::clip`）と観戦コードの書き込み先（`GameUi::spectate_clip`）。
/// LANの接続ではコードは運び手へ運び、観戦コード（Ctrl+C）だけを人のクリップボードへ書く。
/// 人のクリップボードを開けなければ、観戦コードのコピーだけができない。接続は続ける。
type CodeClips = (Box<dyn Clipboard>, Option<Box<dyn Clipboard>>);
fn code_clips(
    lan: Option<&Arc<LanBridge>>,
    open: impl FnOnce() -> Result<Box<dyn Clipboard>, String>,
) -> Result<CodeClips, String> {
    let Some(bridge) = lan else {
        return Ok((open()?, None));
    };
    let spectate = match open() {
        Ok(c) => Some(c),
        Err(e) => {
            game_net_note(format!(
                "LAN: 観戦コードをコピーするクリップボードを開けない（{e}）"
            ));
            None
        }
    };
    Ok((Box::new(LanClip(Arc::clone(bridge))), spectate))
}

/// ネットワークを開く（開くスレッド。`code_side = game`）。開くのは受信ソケット・STUN・監視の処理・ネットワークのセッション。
/// `open_net`の接続コードの形と同じ手順で、コードのフォルダへは触らない（`clipboard = file`のときだけコードのフォルダの下のファイルを使う）。
/// 戻り値は組み込むネットワークのセッション。
fn open_game_net(lazy: &'static LazyNet) -> Result<Box<NetSlot>, String> {
    // LANの接続の頼みがあれば（運び手が出した）その設定とコードの運び先で開く。ほかの接続は今までどおり。
    let lan = LAN_OPEN.lock().unwrap_or_else(|e| e.into_inner()).take();
    let named = match &lan {
        Some(l) => lan_net_settings(lazy.settings_now(), l.ip, l.scope, Some(l.role)),
        None => lazy.settings_now(),
    };
    let s = &named;
    // 役割の乱数は開くたびに生成する（iniに書いてあればその値。再接続でも同じ値）。
    let nonce = s.role_nonce.unwrap_or_else(default_nonce);
    let rb_wired = crate::rollback_play_win::armed();
    let cfg = play_config(s, rb_wired);
    let lan_bridge = lan.map(|l| l.bridge);
    let (clip, spectate_clip) = code_clips(lan_bridge.as_ref(), || open_clipboard(s, lazy))?;
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
        // メニューで選んだ形を自分のプロセス番号と関係なく採る（ファイルを経由しない）ので`pid`は照合に使わない。
        net_code::ModeCtx::new(s, std::process::id(), rb_wired),
        GameUi {
            link: Arc::clone(&lazy.link),
            clip,
            // ルームサーバー: OSのプロキシの設定に従う製品用の既定（`RoomOptions::product`）。
            room: s.room_server.clone().map(RoomOptions::product),
            // ルームへ入った名を`room`に書くini。
            ini_path: lazy.ini_path.clone(),
            // 再接続: 前の接続で使ったコードは読まない。
            spent: spent_sessions(),
            lan: lan_bridge,
            spectate_clip,
        },
    )?;
    let local = local_addr.map_or("（分からない）".to_string(), |a| a.to_string());
    let stun = watch.facts().stun;
    // 前の接続の監視の処理（止めてある。文は後始末したときに本ログへコピーしてある）と置き換える。
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
    // 1回目の行の文字は今までどおり。
    let nth = LINKS_CLOSED.load(Ordering::SeqCst) + 1;
    let how = if nth == 1 {
        "最初の操作で開きました".to_string()
    } else {
        format!("操作で開き直しました（{nth} 回目の接続）")
    };
    game_net_open_note(format!(
        "ネットワーク: ゲーム内のメニューの{how}。受信ソケット {local}（role_nonce {nonce}。待ちは{}）{note}",
        if timer {
            "高分解能の待機タイマー"
        } else {
            "イベントだけ（ms に丸められる）"
        }
    ));
    Ok(Box::new(NetSlot { np, net }))
}

/// ネットワークを開く処理の本体（開くスレッド）。失敗・パニックは理由を本ログの文（`game_net_note`）へ書く。
/// ゲームのスレッドへは「失敗した」だけを渡す（ゲームのスレッドで`Mutex`を待ったり文字列を作ったりしないため）。
fn open_job(open: impl FnOnce() -> Result<Box<NetSlot>, String>) -> Result<Pending, ()> {
    OPEN_TRIES.fetch_add(1, Ordering::Relaxed);
    let r = catch_unwind(AssertUnwindSafe(open))
        .unwrap_or_else(|_| Err("ネットワークを開く処理でパニックしました".to_string()));
    match r {
        Ok(slot) => Ok(Pending::Play(slot)),
        Err(why) => {
            game_net_problem(format!(
                "ネットワーク: 開けませんでした（{why}）。メニューからやり直せます。"
            ));
            Err(())
        }
    }
}

/// 観戦者のネットワークを開く処理の本体（開くスレッド。`open_job`の観戦者バージョン）。失敗・パニックは理由を本ログの文へ書く。
/// ゲームのスレッドへは「失敗した」だけを渡す。
fn open_watch_job(open: impl FnOnce() -> Result<Box<WatchNetSlot>, String>) -> Result<Pending, ()> {
    OPEN_TRIES.fetch_add(1, Ordering::Relaxed);
    let r = catch_unwind(AssertUnwindSafe(open))
        .unwrap_or_else(|_| Err("観戦者のネットワークを開く処理でパニックしました".to_string()));
    match r {
        Ok(slot) => Ok(Pending::Watch(slot)),
        Err(why) => {
            game_net_problem(format!(
                "観戦: 観戦者のネットワークが開けませんでした（{why}）。メニューからやり直せます。"
            ));
            Err(())
        }
    }
}

/// 観戦用のルーム名と、左上に出す名（ルーム名の代わり）を返す。
/// 一覧から選んだ行があればその行から作る（観戦用のルーム名はハッシュから戻し、左上は「1P名 vs 2P名」）。
/// 無ければ観戦の「ルームマッチ」で打った名を使う。
fn watch_target(link: &UiLink) -> Result<(higanr_core::room::RoomName, String), String> {
    if let Some(row) = link.spectate_pick() {
        let room = row
            .room_name()
            .ok_or("一覧から選んだ行の観戦用のルーム名が規則の範囲外")?;
        return Ok((room, format!("{} vs {}", row.p1.as_str(), row.p2.as_str())));
    }
    let buf = link
        .spectate_name()
        .ok_or("ルーム名が未設定（メニューは名を設定してから指示を送る）")?;
    // メニューも同じガード（`RoomName::parse`）で見て渡すが、ここでも見直す（`RoomNameError`のDebugは位置と数だけで、名の文字を含まない）。
    let name = buf
        .parse()
        .map_err(|e| format!("ルーム名が規則の範囲外: {e:?}"))?;
    Ok((name.spectate_of(), name.as_str().to_string()))
}

/// 観戦者のネットワークを開く（開くスレッド）。開くのは受信ソケット・STUN・受信スレッド・観戦者のセッション・観戦者のルームのスレッド。
/// 接続コードの監視の処理は作らない（観戦用のコードはルームが運ぶ）。
/// 決定済みのルーム名は`UiLink::spectate_name`から読み、**本ログには書かない**。
fn open_watch_net(lazy: &'static LazyNet) -> Result<Box<WatchNetSlot>, String> {
    // LANの観戦: 運び手が受けた観戦コードを使いLANの設定（範囲lan・自分のLANのアドレス・STUNなし）で開く。
    // 古い（別の観戦のために出した）ものは使わない。
    let lan = LAN_WATCH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .filter(|w| w.at.elapsed() < LAN_WATCH_FRESH);
    let named = match &lan {
        Some(w) => lan_net_settings(lazy.settings_now(), w.ip, w.scope, None),
        None => lazy.settings_now(),
    };
    let s = &named;
    // 観戦コード入力: ルームサーバーは使わない。クリップボードはここで1回だけ読む。
    // 読めたかどうかの判断は観戦者のスレッドが状態に書く。
    let by_code = lan.is_some() || lazy.link.spectate_by_code();
    let (source, shown) = if let Some(w) = lan {
        (JoinSource::Code(Ok(Some(w.code))), String::new())
    } else if by_code {
        let read = open_clipboard(s, lazy)?.read();
        (JoinSource::Code(read), String::new())
    } else {
        let (watch_room, shown) = watch_target(&lazy.link)?;
        let base = s
            .room_server
            .clone()
            .ok_or("ルームサーバー（room_server）がありません（観戦用のルームへ入れません）")?;
        (
            JoinSource::Room {
                room: RoomOptions::product(base),
                watch_room,
            },
            shown,
        )
    };
    let glyphs = higanr_core::overlay::ui_glyphs();
    let mut publisher = Publisher::new(
        Arc::clone(&lazy.link),
        sanitize_room(&shown, |c| glyphs.binary_search(&c).is_ok()),
        net_udp::now_unix() as u32,
    );
    publisher.publish(
        CodeState::SpectateJoining,
        if by_code {
            Event::None
        } else {
            Event::RoomJoining
        },
    );
    // 役割の乱数はJOINのnonceに使う（観戦者の識別。iniに書いてあればその値）。
    let nonce = s.role_nonce.unwrap_or_else(default_nonce);
    let (bind, bind_note) = net_udp::resolve_bind(s)?;
    let (mut net, recv) = net_udp::open_for_code_scoped(s.scope, bind)?;
    let local_addr = net.local_addr();
    let local = local_addr.ok_or("受信ソケットが不明")?;
    let timer = net.high_resolution_wait();
    // 観戦者の候補（返答に載せる）: 自分の受信ソケット・iniの`stun`があれば見えたアドレス・開けたIPv6のソケット。
    // 観戦の相手（対戦のホストか中継役）のIPv4・IPv6の候補の全部へJOINを送る。
    // 最初にSTREAMが返った経路に決める（`Watcher`）。
    let mut note = String::new();
    // 中継する観戦者はUPnPで受信ポートの割り当ても頼む（中継役からの穴あけの補助）。
    // STUNと並行に走らせて結果は待たない（受信スレッドがループのたびに確認する）。
    let relay = s.relay && source.can_relay();
    let upnp = relay && net_udp::upnp_begin(s, &net, &mut note);
    let (mapped, stun) = net_udp::stun_state(s, &mut net, &mut note);
    let v6 = net_udp::open_ipv6(s, &net, &mut note);
    let own = s
        .candidates_for(local, mapped, v6)
        .map_err(|e| format!("{e}{note}"))?;
    net.note_own_candidates(own.as_slice());
    if upnp {
        net.plan_watch_upnp(WatchUpnpPlan::new(s.candidates, local, mapped, v6));
        note.push_str(" / 観戦者は UPnP の結果を待たずに接続します（結果は接続の後に確認）");
    }
    let slot = Arc::new(WatchSlot::new());
    if relay {
        slot.enable_relay(own);
    }
    let wp = WatchPlay::try_new(
        Arc::clone(&slot),
        WatchConfig {
            wait_budget_us: s.wait_budget_us,
            margin_frames: WATCH_MARGIN_FRAMES,
            // 本物のゲームは選択の画面を持つ。
            require_net_selection: true,
            ff_max: s.ff_max,
        },
    )?;
    let kit = JoinKit {
        source,
        reader: Reader::new(s, nonce).with_v6(v6.is_some()),
        reply_name: higanr_core::handshake::Name::new(&s.name),
        own,
        slot,
        publisher,
    };
    // 作れなければ`net`・`recv`がここで破棄されて受信ソケットが閉じる。
    spectate_join::spawn(kit)?;
    *OPEN_INFO.lock().unwrap_or_else(|e| e.into_inner()) = Some(OpenInfo {
        local: local_addr,
        scope: s.scope,
        stun,
    });
    *RECV.lock().unwrap_or_else(|e| e.into_inner()) = Some(recv);
    start_relink_thread();
    // 観戦者の早送り: 観戦するマッチの方式は接続するまで分からない。
    // 早送りは方式に依らないので観戦者のネットワークを開くときに走査のcallを書き込む。
    // 書き込めなくても観戦は続く（早送りが効かないだけ）。
    game_net_note(format!("観戦: 早送り: {}", arm_watch_scan_at_open()));
    let nth = WATCHES_CLOSED.load(Ordering::SeqCst) + 1;
    game_net_note(format!(
        "観戦: {}で観戦者のネットワークを開いた（{nth} 回目。ルーム名・コードは記録しない）。受信ソケット {local}（待ちは{}）{bind_note}{note}",
        if by_code {
            "「観戦コード入力」"
        } else {
            "「ルームで観戦」"
        },
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

/// 開き終わったネットワークのセッションがあれば取って組み込む（ゲームのスレッド。ネットワークを開いている最中だけ呼ぶ）。
/// ロックが取れなければ待たずに次のtickへ回す。
/// 組み込めなかった・開けなかったらメニューへ知らせて`OPEN_IDLE`に戻す。
/// ゲームのスレッドの規律: `try_lock`を使う・文字列を作らない・確保しない（例外は失敗したネットワークのセッションの解放）。
/// 失敗の文は開くスレッド（`open_job`）が書く。
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
    let mut handed = false;
    let placed = match r {
        Ok(Pending::Play(slot)) => install_slot(slot),
        Ok(Pending::Watch(w)) => match install_placed(Slot::Watch(w)) {
            Ok(()) => true,
            Err(slot) => {
                handed = hand_off_unplaced(slot);
                false
            }
        },
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
    if !handed {
        OPEN_STATE.store(OPEN_IDLE, Ordering::SeqCst);
    }
}

/// 組み込めなかった観戦者のセッションをワーカーのスレッドの後始末（`relink_tick` → `close_watch`）へ回す。
/// 受け渡しのリングの約8.8 MBをゲームのスレッドで解放しないため。
/// 回せたらtrue（状態は`OPEN_CLOSING`）。
/// 郵便受けが塞がっているときはここで解放する。確保しない・待たない。
fn hand_off_unplaced(slot: Slot) -> bool {
    let Slot::Watch(w) = slot else {
        return false;
    };
    if RELINK_STARTED.load(Ordering::Acquire) == 0 {
        return false;
    }
    let Some(lazy) = LAZY.get() else {
        return false;
    };
    let Ok(mut mailbox) = CLOSING.try_lock() else {
        return false;
    };
    if mailbox.is_some() {
        return false;
    }
    let link = Arc::clone(&lazy.link);
    let mark = link.order_mark();
    *mailbox = Some(Closing {
        slot: Released::Watch(w),
        link,
        mark,
    });
    OPEN_STATE.store(OPEN_CLOSING, Ordering::SeqCst);
    true
}

/// ネットワークのセッションを`NET`へ組み込んで動かす（ゲームのスレッド。`NET_ON`が0のとき）。組み込めたらtrue。
/// アトミック変数だけで決まるチェック（`not_ready`）を先にする。
/// 直接読むアドレスのチェックは`arm_game_ui`が起動時に済ませてある。
fn install_slot(slot: Box<NetSlot>) -> bool {
    install_placed(Slot::Play(slot)).is_ok()
}

/// `install_slot`の中身（対戦のセッションも観戦者のセッションも同じ手順）。
/// 観戦者のセッションでは、読み取りのスレッドが`_watch_frames.csv`を開くためのフラグ（`spectate_watch::set_armed`）を先に立て、先頭のフックは設定しない。
fn install_placed(slot: Slot) -> Result<(), Slot> {
    if NET_ON.load(Ordering::SeqCst) != 0
        || not_ready(SET_STATE_FN.load(Ordering::SeqCst)).is_some()
    {
        INSTALL_FAILS.fetch_add(1, Ordering::Relaxed);
        return Err(slot);
    }
    // SAFETY: 引数の無い呼び出し。
    GAME_TID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    // ネットワークの数をセッションごとに0から数え直す（再接続の2本目から意味がある）。
    // `publish`はCHECKのズレのフィールドを上書きしないので、消さないと次の接続の行に残る。
    // マッチをやめた数もセッションごとに0から数える。
    // 確保も待ちも無い（ネットワークのセッションを組み込むtickに1回。マッチの外）。
    NET_STATUS.reset();
    for a in [&RB_ABORTS, &RB_ABORT_FIRST, &RB_ABORT_FRAME] {
        a.store(0, Ordering::Relaxed);
    }
    let play = matches!(slot, Slot::Play(_));
    if !play {
        // 検査では立てない（静的変数が残るので、ほかの検査の`_net_frames.csv`の隣に`_watch_frames.csv`が現れる）。
        #[cfg(not(test))]
        crate::spectate_watch::set_armed();
    }
    // SAFETY: `NET_ON`が0なのでネットワークのセッションに触るもう一方（`route`・`match_start_on`・`rb_slot`）は`NET_ON`を見て触らない。
    // ここはゲームのスレッドで、触るのもゲームのスレッドだけ。
    unsafe { *NET.0.get() = Some(slot) };
    NET_ARMED.store(1, Ordering::SeqCst);
    if play {
        install_head_wait();
    }
    NET_ON.store(1, Ordering::Release);
    OPEN_STATE.store(OPEN_DONE, Ordering::SeqCst);
    Ok(())
}

/// メニューのコンテナ（ゲームのスレッドだけが触る。メニューが無い実行は`None`）。tickの中で借りてそのtickの中で返す。
fn game_ui() -> Option<&'static mut NetUi> {
    if UI_ON.load(Ordering::Acquire) == 0 {
        return None;
    }
    // SAFETY: `UI_ON`をAcquireで1と読んだので`arm_game_ui`が設定したコンテナが見える。
    // 触るのはゲームのスレッドだけ。借りた参照は呼び出したtickの中で使い終える。
    unsafe { (*UI.0.get()).as_mut() }
}

/// メニューのカウンタ（`UiStats`）を、読み取りのスレッドが本ログへ読むアトミック変数へコピーする。
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

/// ネットワークをまだ開いていない実行（`np`なし）のメニューを1 tick進める（ゲームのスレッド）。
/// `UiOut::action`が`Some`ならメニューが入力を占有してfeedを済ませている。
/// 最初の「作る」「貼る」のときはここでネットワークを開く仕事を依頼する（`request_open`）。
fn ui_only_step(game: &mut NetGame<'_>) -> UiOut {
    let Some(ui) = game_ui() else {
        return UiOut::default();
    };
    // 後始末中（`OPEN_CLOSING`）は「閉じている」と見せる。
    // 人が「作る」を選ぶと開く依頼になり、再接続のスレッドが後始末を終えてから開く。
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
    return_save_quit_tick(&out);
    if out.action.is_some() || out.open_net {
        publish_ui_stats(ui);
    }
    if out.open_net {
        request_open();
    }
    if spectate_asked(&out) {
        request_spectate();
    }
    if let Some(ask) = out.lan {
        request_lan(ask);
    }
    lan_watch_poll();
    out
}

/// マッチ中の行を更新する（ゲームのスレッド。`h`が`None`なら消す）。
/// 材料（`NetPlay::hud_facts`）は進め直しの数の差を取るので呼ぶと状態が動く。
/// そのため**メニューを組み込んでいる実行（`UI_ON`）の中だけ**で作る: offer / answerの実行に0の上乗せをしない。
fn ui_hud_tick(np: &mut NetPlay) {
    let Some(ui) = game_ui() else {
        return;
    };
    match np.hud_facts() {
        Some(h) => ui.hud(true, h),
        None => ui.hud(false, HudFacts::default()),
    }
    ui.peer_wait(np.peer_wait_shown());
}

/// ネットワークをまだ開いていない実行の1 tick。
/// 画面6以外でメニューが入力を占有しないtickは、台本があれば台本へ回し、無ければ 0 = poll_all へ返す。
/// 画面6ではメニューが必ず入力を占有する（`NetUi::tick`は画面6では常にactionを返す）。
/// そのため画面6・カーソル4の決定がゲームへ漏れる経路は無い。
/// ネットワークのセッションを組み込んだtickもここへ来ない（`route`が先に組み込んでネットワークの経路へ回す）。
fn ui_only_tick(mut engine: Option<&mut Engine>, io: &mut RealTickIo, k: u32) -> u32 {
    if NET_ON.load(Ordering::Acquire) != 0 {
        // ここには来ない見込み（`route`が`NET_ON`を見てから呼ぶ）。
        // 来たらゲーム自身のpollへ任せない（画面6・カーソル4の決定が漏れうる）。
        // 0を3本（サイド0〜2）ゲームの入力へ渡してpoll_allを呼ばずに戻る。
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
        // メニューが台本を1 tick進めた（自分の入力ワードに使った）。撮影の依頼と進み具合は`net_tick`と同じに出す。
        if s.shot != 0 {
            shot_win::place_request(s.shot, s.step, k);
        }
        publish(s.status);
    }
    match out.action.and_then(net_game::entry_return) {
        Some(ret) => ret,
        None => {
            if let Some(ret) = split_keyboards_tick(&mut engine, io, k) {
                return ret;
            }
            match engine {
                Some(e) => script_tick(e, io, k, true),
                None => 0,
            }
        }
    }
}

/// ネットワークのセッションを開いている最中かどうか（`ui_only_step`が見せる`NetPhase::Opening`と同じ）。
fn net_opening() -> bool {
    matches!(OPEN_STATE.load(Ordering::Relaxed), OPEN_BUSY | OPEN_REOPEN)
}

/// ローカルの人間 対 人間でキーボード2台を1P・2Pに分ける（`ui_only_tick`の中でメニューが入力を占有しないtick）。
/// 入力を占有したら入口への戻り値を返し、占有しなければ`None`を返す（呼び出し側は今までどおりの入力へ進む）。
/// 台本が動いていれば1 tick進めて、偽の押下の命令（`kbd=`）を表へ書き込む。
/// 分けている間は台本のワードの代わりに台ごとのワードをfeedする。
/// 台本の実行は本物のキーを登録も読みもしない。
fn split_keyboards_tick(
    engine: &mut Option<&mut Engine>,
    io: &mut RealTickIo,
    k: u32,
) -> Option<u32> {
    let ui = game_ui()?;
    if !ui.split_on() {
        return None;
    }
    if engine.is_none() {
        crate::split_kb_win::ensure_registered();
    }
    let mut stepped = None;
    if let Some(e) = engine.as_deref_mut()
        && matches!(e.status(), Status::Running { .. })
    {
        let s = scripted::advance(e, io, &load_addrs());
        if let Some(raw) = e.take_raw() {
            crate::split_kb_win::inject(&raw);
        }
        stepped = Some(s);
    }
    let act = {
        let mut game = NetGame::new(io, engine.as_deref_mut(), load_addrs());
        ui.split_step(&mut game, &crate::split_kb_win::TABLE, net_opening())
    };
    let (w1, w2) = match act {
        higanr_core::split_kb::Act::Pass => {
            return stepped.map(|s| script_commit(io, k, &s, true, true));
        }
        higanr_core::split_kb::Act::Hold(_) => (0, 0),
        higanr_core::split_kb::Act::Feed { w1, w2 } => (w1, w2),
    };
    match stepped {
        Some(mut s) => {
            s.out.feed = Some((w1, w2));
            Some(script_commit(io, k, &s, true, true))
        }
        None => {
            for (i, w) in [w1, w2, w1 | w2].into_iter().enumerate() {
                io.feed(i as u32, u32::from(w));
            }
            Some(1)
        }
    }
}

/// 台本のコンテナとアドレスを設定する（読み取りのスレッド。`SCRIPT_ON`が0の間だけ）。
/// 直接読むアドレスが読み取れない・関数のアドレスが0のときは断る（設定したものは使われない）。
pub fn arm(script: Script, addrs: GameAddrs, feed_fn: u32, local_fn: u32) -> Result<(), String> {
    arm_io(addrs, feed_fn, local_fn)?;
    // SAFETY: `SCRIPT_ON`が0なのでゲームのスレッドはコンテナに触らない（`EngineCell`のSyncの注。`arm_io`が見た）。
    unsafe { *ENGINE.0.get() = Some(Engine::new(script)) };
    HAS_SCRIPT.store(1, Ordering::SeqCst);
    STATE.store(RunState::Running.code(), Ordering::SeqCst);
    Ok(())
}

/// アドレスと関数のアドレスだけを設定して台本のコンテナは空にする（台本の無いネットワークの実行。`SCRIPT_ON`が0の間だけ）。断る条件は`arm`と同じ。
/// 入力の入口を開けるのは、この後にネットワークのセッションを組み込んで動かしたときだけ（`start`）。
pub fn arm_io(addrs: GameAddrs, feed_fn: u32, local_fn: u32) -> Result<(), String> {
    if SCRIPT_ON.load(Ordering::SeqCst) != 0 {
        return Err("台本が既に動作中".to_string());
    }
    if feed_fn == 0 || local_fn == 0 {
        return Err(format!(
            "呼ぶ関数のアドレスが 0（feed 0x{feed_fn:08X} / ローカル入力 0x{local_fn:08X}）"
        ));
    }
    for (a, len) in addrs.direct_ranges() {
        if a == 0 || read_own_memory(a, len as usize).is_none() {
            return Err(format!(
                "直接読むアドレス 0x{a:08X}（{len} バイト）が読み取り不能"
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
    // 前の実行のpollのアドレスを持ち越さない（設定するのはこの後に照合が合ったときの`arm_poll`だけ）。
    POLL_FN.store(0, Ordering::SeqCst);
    for (a, v) in STATE_SLOTS.iter().zip(PLAYER_SLOTS) {
        a.store(v, Ordering::SeqCst);
    }
    // `SCRIPT_ON`が0なのでゲームのスレッドは記録に書いていない。
    STATE_LOG.reset();
    // SAFETY: `SCRIPT_ON`が0なのでゲームのスレッドはコンテナに触らない（`EngineCell`のSyncの注）。
    unsafe { *ENGINE.0.get() = None };
    HAS_SCRIPT.store(0, Ordering::SeqCst);
    for a in [&STEP, &LINE, &TICKS, &FED] {
        a.store(0, Ordering::SeqCst);
    }
    STATE.store(RunState::Idle.code(), Ordering::SeqCst);
    Ok(())
}

/// 入口の分岐を開く（`SCRIPT_ON` = 1）。関数のアドレスが設定されていなければ開かない。
/// 台本のコンテナが空ならネットワークが動いているときだけ開く（台本もネットワークも無いのに開くと入口は毎tick Rustを呼ぶだけになる）。
pub fn start() -> Result<(), String> {
    if FEED_FN.load(Ordering::SeqCst) == 0 || LOCAL_FN.load(Ordering::SeqCst) == 0 {
        return Err("関数のアドレスが未設定（arm / arm_io の前）".to_string());
    }
    // SAFETY: 読むだけ。`SCRIPT_ON`を1にする前なのでゲームのスレッドはコンテナに触らない。
    if unsafe { (*ENGINE.0.get()).is_none() }
        && NET_ON.load(Ordering::SeqCst) == 0
        && UI_ON.load(Ordering::SeqCst) == 0
    {
        return Err(
            "台本の入れ物が空で、ネットワークもメニューも動作していません（arm か start_net の前）"
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

/// ネットワークのセッションとシードを設定する関数のアドレスを設定する（読み取りのスレッド。`SCRIPT_ON`と`NET_ON`が0の間だけ）。
/// 次のどれかなら断る: 入力の関数のアドレスや描画だけのフレームの出口が設定されていない・直接読み書きするアドレスが読み取れない・関数のアドレスが0。
pub fn arm_net(slot: NetSlot, set_state_fn: u32) -> Result<(), String> {
    arm_net_with(slot, set_state_fn, &net_game::direct_ranges())
}

/// 観戦者のセッションを組み込む（`arm_net`の観戦者バージョン。断る条件は同じ）。
pub fn arm_watch(slot: WatchNetSlot, set_state_fn: u32) -> Result<(), String> {
    arm_slot(
        Slot::Watch(Box::new(slot)),
        set_state_fn,
        &net_game::direct_ranges(),
    )
}

/// `arm_net`の中身（読み取れるかをチェックする範囲を渡す。検査では自分の静的変数か確保されていないアドレスを渡す）。
fn arm_net_with(slot: NetSlot, set_state_fn: u32, ranges: &[(u32, u32)]) -> Result<(), String> {
    arm_slot(Slot::Play(Box::new(slot)), set_state_fn, ranges)
}

/// ネットワークのセッションを組み込んでよい準備が整っているかどうか（関数のアドレス・描画だけのフレームの出口・直接読み書きするアドレス）。
/// `arm_slot`と`arm_game_ui`（ネットワークを開く前にメニューを組み込むとき）と`install_slot`（後からセッションを組み込むとき）が同じチェックをする。
fn check_ready(set_state_fn: u32, ranges: &[(u32, u32)]) -> Result<(), String> {
    match not_ready(set_state_fn) {
        None => {}
        Some(NotReady::Fn) => {
            return Err("関数のアドレスが 0（set_state か、arm / arm_io の前）".to_string());
        }
        Some(NotReady::Exit) => {
            return Err("描画だけのフレームの出口が入口に設定されていません".to_string());
        }
    }
    for &(a, len) in ranges {
        if read_own_memory(a, len as usize).is_none() {
            return Err(format!(
                "直接読み書きするアドレス 0x{a:08X}（{len} バイト）が読み取り不能"
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

/// 関数のアドレスと出口が設定されていなければその理由を返す。確保しない・待たない（ゲームのスレッドの`install_slot`が使う）。
/// 直接読むアドレスのチェックは起動時の`arm_game_ui`とネットワークを開くスレッドの`open_game_net`が済ませる。
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
        return Err("入口の分岐が既に開いています".to_string());
    }
    check_ready(set_state_fn, ranges)?;
    SET_STATE_FN.store(set_state_fn, Ordering::SeqCst);
    GAME_TID.store(0, Ordering::SeqCst);
    // SAFETY: `SCRIPT_ON`と`NET_ON`が0なのでゲームのスレッドはネットワークのセッションに触らない（`NetCell`のSyncの注）。
    unsafe { *NET.0.get() = Some(slot) };
    Ok(())
}

/// 素のpollの入口を設定する（読み取りのスレッド。`arm_io`の後`SCRIPT_ON`を1にする前。0なら「設定しない」）。
/// 呼ぶのは`session::run`が`POLL_SITES`の先頭8バイトをth09.exeの実バイトと照合して合ったときだけ。
pub fn arm_poll(addr: u32) {
    POLL_FN.store(addr, Ordering::SeqCst);
}

/// 立ち絵の3本の関数の入口を設定する（読み取りのスレッド。`arm_net`の後`NET_ON`を1にする前）。
/// 3本とも0でないときだけ設定する。1本でも0なら全部0に戻す（一部だけ呼ぶ形を作らないため）。
pub fn arm_portrait(fns: [u32; 3]) {
    let ok = fns.iter().all(|&a| a != 0);
    for (slot, a) in PORTRAIT_FNS.iter().zip(fns) {
        slot.store(if ok { a } else { 0 }, Ordering::SeqCst);
    }
}

/// 受信スレッド（開いたら閉じない。プロセスの終わりまで残す）。
/// ゲーム内のメニューの実行だけは、接続が終わるとワーカーのスレッドが取り出して止め（`relink_tick`）、次の接続を開くスレッドが新しいものを設定する。
/// ゲームのスレッドが触るのはパニックを受けたときの`try_lock` 1回だけ（`bye_after_panic`）。
static RECV: std::sync::Mutex<Option<RecvThread>> = std::sync::Mutex::new(None);

/// ゲームスレッドの経路でパニックを受けた後に受信スレッドへBYEを出させる（つながっている相手は60秒の沈黙を待たずに切れる）。
/// ロックは待たない: 取れなければ何もしない。
/// その場合の相手は、心拍が古くなってPONGが止まってから切れる。
fn bye_after_panic() {
    #[cfg(test)]
    PANIC_BYES.fetch_add(1, Ordering::SeqCst);
    let recv = match RECV.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    if let Some(r) = recv.as_ref() {
        r.say_bye(higanr_core::wire::ByeReason::Cancelled);
    }
}

/// 検査だけ: `bye_after_panic`を呼んだ回数。
#[cfg(test)]
pub(crate) static PANIC_BYES: AtomicU32 = AtomicU32::new(0);

/// ゲームスレッドの経路（入力の入口・開始のフック・rollbackの走査とPresentの後のフック）でパニックを受けた後始末。
/// ネットワークの分岐を閉じて受信スレッドにBYEを出させる。
/// ネットワークが動いていなかった（もう閉じた）ときは何もしない。
pub(crate) fn net_panicked() {
    if NET_ON.swap(0, Ordering::SeqCst) != 0 {
        NET_PANICKED.store(1, Ordering::SeqCst);
        bye_after_panic();
    }
}

/// ネットワークのセッションを組み込んだ（読み取りのスレッドが`_net_frames.csv`を開くかどうかのフラグ）。
static NET_ARMED: AtomicU32 = AtomicU32::new(0);

/// 役割を決める乱数の既定（iniに`nonce`が無いとき。時刻とプロセス番号から。peer_simの既定と同じ考え方）。
fn default_nonce() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut z = t ^ (u64::from(std::process::id()) << 32);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z ^ (z >> 31)
}

/// 接続コードの監視の処理（iniの`code_side`の形だけ）。プロセスの終わりまで持つ（接続した・ネットワークが終わった・諦めたときは自分で終了する）。
/// ゲーム内のメニューの実行では、接続を後始末するときに止めて（文と数は残す）次の接続を開くときに置き換える。
static CODE_WATCH: std::sync::Mutex<Option<net_code::Watch>> = std::sync::Mutex::new(None);
/// 観戦用のコードの監視の処理（iniの`code_side = spectate`の形だけ。`WATCH_CODE`も終わりまで持つ）。
static WATCH_CODE: OnceLock<spectate_watch_code::WatchCode> = OnceLock::new();
/// ネットワークを開いたときに分かったこと（受信ソケットのアドレス・範囲・STUNの結果。試験のまとめ）。
/// ゲーム内のメニューの実行では、接続を開くたびに置き換える（最後の接続の値）。
static OPEN_INFO: std::sync::Mutex<Option<OpenInfo>> = std::sync::Mutex::new(None);

/// ネットワークを開いてセッションを組み込む（読み取りのスレッド）。まだ動かさない（`start_net`）。開いた受信ソケットの文を返す。
/// `code_dir`は接続コードのフォルダ（`RealScriptHost::arm_net`がゲームのフォルダの外かを見てから渡す）。
pub fn open_net(
    s: &NetSettings,
    set_state_fn: u32,
    code_dir: Option<PathBuf>,
    spectate_dir: Option<PathBuf>,
) -> Result<String, String> {
    if RECV.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return Err("ネットワークは既に開いています".to_string());
    }
    let nonce = s.role_nonce.unwrap_or_else(default_nonce);
    // 接続コードの形で監視の処理を作る前に断ったときは、ウィンドウの「状態」の行に出す文をコードのフォルダへ書く（`net_code::say_not_started`）。
    let status_dir = code_dir.clone();
    let not_started = |e: String| {
        if let (Some(side), Some(d)) = (s.code_side, status_dir.as_deref()) {
            net_code::say_not_started(d, side);
        }
        e
    };
    // `bind = auto:ポート`はここ（読み取りのスレッド）で経路表を参照してアドレスにする。決められなければネットワークを開かない。
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
        // 観戦者: 対戦のセッションは作らない。観戦用のコードの監視の処理と`WatchPlay`を組み込む。
        let Some(dir) = code_dir else {
            return Err(not_started("接続コードの場所が未決定".to_string()));
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
        let wp = WatchPlay::try_new(
            slot,
            WatchConfig {
                wait_budget_us: s.wait_budget_us,
                margin_frames: WATCH_MARGIN_FRAMES,
                // 本物のゲームは選択の画面を持つ。
                require_net_selection: true,
                ff_max: s.ff_max,
            },
        )
        .map_err(not_started)?;
        // 断られたら`th`と監視の処理（`w`）はここで破棄されて止まる。
        arm_watch(
            WatchNetSlot {
                wp,
                net,
                shots: Default::default(),
            },
            set_state_fn,
        )?;
        // 読み取りのスレッドが`_watch_frames.csv`を開くためのフラグ（`script_watch`）。検査の`arm_watch`では立てない（静的変数が残るため）。
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
    // rollbackの入口を全部書き込めた実行かどうか。HELLOで名乗る形（iniの形もウィンドウで選んだ形も）はこれを見る。
    let rb_wired = crate::rollback_play_win::armed();
    let cfg = play_config(s, rb_wired);
    // 秘密の乱数（iniかOSの乱数）・STUN・セッションの設定は通信層で作る（`net_udp::prepare_link`）。
    // 接続コードの形ではセッションの設定を監視のスレッドが相手のコードを読んでから書く。
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
                // 接続コードのウィンドウで選んだ形（`mode.txt`）を自分のプロセス番号と合うときだけ採る（前の実行の残りを使わない）。
                net_code::ModeCtx::new(s, std::process::id(), rb_wired),
            )
            .map_err(not_started)?;
            // STUNの結果は監視の処理が持つ（受信ソケットを開くときに問い合わせた）。
            let stun = w.facts().stun;
            (
                NetPlay::with_code_slot(slot, cfg, &mut net),
                note,
                Some(w),
                stun,
            )
        }
        (Some(_), None) => return Err("接続コードの場所が未決定".to_string()),
    };
    // 観戦のホスト（iniの`[spectate] enable = 1`）: リングバッファをセッションに付けて観戦用のコードを用意する。使わない設定なら何もしない。
    let mut np = np;
    let (spectate, spectate_note) = spectate_host::arm(s, spectate_dir, local_addr, &mut np);
    // 断られたら`th`と監視の処理（`watch`）はここで破棄されて止まる。
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

/// 試験のまとめの材料のうち静的な値から取得するもの（読み取りのスレッド。ネットワークを開いていなければ`open`と`code`はNone）。
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

/// ゲーム内のメニューの実行でネットワークを開く段階（読み取りのスレッド）。
/// 依頼が来たかどうかは、開くスレッドの数（`OPEN_TRIES`）かスレッドを作れなかった文（`game_net_note`）で判断する。
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

/// ネットワークの数とネットワークの外の数の文（読み取りのスレッド。ログ用）。1秒ごとに呼ばれるのでマッチの終わりの選択のイベントもここで本ログへ渡す。
pub fn net_status() -> (NetSnapshot, String) {
    crate::end_outline_win::flush_notes();
    let (recv, spec, spectators, diag) = {
        let g = RECV.lock().unwrap_or_else(|e| e.into_inner());
        // 観戦のホストの集計（`[spectate] enable = 1`のときだけ1行。無ければ今までの行のまま）。
        let host = g.as_ref().and_then(RecvThread::spectate_snapshot);
        (
            g.as_ref().map_or("受信スレッドなし".to_string(), |t| {
                format!("受信 {:?}", t.stats())
            }),
            host.map(|s| format!("\n{}", s.lines(crate::log_mode::detail())))
                .unwrap_or_default(),
            host.map(|s| {
                let c = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
                crate::net_watch::SpectatorCounts {
                    now: s.spectators,
                    joined: c(s.host.joined),
                    left: c(s.host.left),
                    silenced: c(s.host.silenced),
                    refused_full: c(s.host.refused_full),
                }
            }),
            // 受信側が持つ診断（STUNの聞き直し・送信の誤り・自分の候補。受信スレッドが無ければ全部0 / なし）。
            g.as_ref().map(RecvThread::diag).unwrap_or_default(),
        )
    };
    // 後始末した接続の監視の文は、後始末したときに「n 回目の接続」の節へコピーしたのでここでは出さない。
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
    // ゲーム内のメニューの実行: ネットワークを開く仕事の文とメニューのカウンタ（`code_side = game`の実行だけ）。
    let code = GAME_NET_NOTES
        .lock()
        .map(|g| g.iter().map(|l| format!("\n{l}")).collect::<String>())
        .unwrap_or_default()
        + &code
        + &ui_stats_text()
        + &crate::end_outline_win::stats_text();
    // 観戦者（`code_side = spectate`）の監視の文と集計（観戦者の実行だけ。無ければ今までの行のまま）。
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
        // 観戦者のルーム（「ルームで観戦」。観戦用のルームのイベントの数。作ったことが無ければ空）。
        + &spectate_requests_text()
        + &spectate_join::stats_text()
        + &PAST_WATCHES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .concat();
    // ゲーム内のメニューの実行では、接続が終わってもネットワークを開き直せる。
    // このとき`NetSnapshot::ended`は「この起動ではもう開かない」の意味になり、計測はネットワークの1本の終わりでは締めない。
    // パニックでネットワークとメニューを止めた実行は開き直せない。
    let mut snap = NET_STATUS.snapshot();
    snap.relink = UI_ON.load(Ordering::SeqCst) != 0;
    snap.diag = diag;
    snap.spectators = spectators;
    // 接続できなかった理由を読むための診断（セッションを作った後・STUNを使った実行だけ。人を特定する値は書かない）。
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

/// 「ルームで観戦」の依頼の数（本ログの1行。依頼が1度も来ていない実行は空）。
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

/// 後始末した接続の節（再接続した実行だけ。本ログの「ネットワークの外」の終わりに足す）と、上のネットワークの行が何回目の接続かの1行。
fn past_links_text() -> String {
    let past = PAST_LINKS.lock().unwrap_or_else(|e| e.into_inner());
    if past.is_empty() {
        return String::new();
    }
    let closed = LINKS_CLOSED.load(Ordering::SeqCst);
    let now = if CODE_NOTES_ARCHIVED.load(Ordering::SeqCst) {
        format!(
            "（上の「ネットワーク:」の行とネットワークの数は {closed} 回目の接続の最後の値です。次の接続はまだ開いていません）"
        )
    } else {
        format!(
            "（上の「ネットワーク:」の行とネットワークの数は {} 回目の接続のぶんです。前回の接続は下の節）",
            closed + 1
        )
    };
    format!("\n{now}{}", past.concat())
}

/// rollbackのモードのカウンタ（本ログの「ネットワークの外」に足す。入口を書き込めていない・一度もコピーしていない実行は空）。
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
    /// マッチをやめた回数・最初の経路（`abort_code`。0はやめていない）・そのときまでに確定したフレームの数。
    aborts: u32,
    first: u32,
    first_frame: u32,
    /// rollbackのマッチの間にフックがコンテナを借りられず走査を普段どおりに回した回数。
    bypass: u32,
}

/// マッチをやめた経路の番号（`RB_ABORT_FIRST`。0はやめていない）。
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

/// マッチをやめた回数の行の先頭（`rollback_text_with`が書き、読み取りのスレッドが簡潔なログへコピーする）。
pub const ROLLBACK_ABORT_HEAD: &str = "巻き戻し（フック）: マッチをやめた";

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
            "\n巻き戻し（フック）: rollback の試合の間に、フックが入れ物を借りられず走査を普段どおりに回した {bypass} 回"
        );
    }
    if nested != 0 {
        s += &format!(
            "\n巻き戻し（製品の処理フロー）: フックが入れ物を持っている間に来た開始のフック {nested} 回（触らなかった）"
        );
    }
    if stray != 0 {
        s += &format!(
            "\n巻き戻し（製品の処理フロー）: フックが入れ物を持っている間に、指図が無いまま来た入力の入口 {stray} 回（poll_all を呼ばずに戻した）"
        );
    }
    s
}

/// ネットワークを動かす（`NET_ON` = 1）。`arm_net`より後、入力の分岐（`start`）より前に呼ぶ。
pub fn start_net() -> Result<(), String> {
    // SAFETY: 読むだけ。`NET_ON`を1にする前なのでゲームのスレッドはネットワークのセッションに触らない。
    if unsafe { (*NET.0.get()).is_none() } {
        if UI_ON.load(Ordering::SeqCst) != 0 {
            // ゲーム内のメニューだけの実行（ネットワークは最初の作る・貼るで開く）。`NET_ON`はネットワークのセッションを組み込むときに立てる。
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
/// 素のpoll `0x0042B850`: ecx = サイド（0〜2）・スタックの引数なし・素の`ret`・戻り値ax（上位16ビットは不定）。
type PollFn = unsafe extern "fastcall" fn(u32) -> u32;
type SetStateFn = unsafe extern "thiscall" fn(u32, u32);
/// 立ち絵の3本（`0x00424EDD` / `0x00424FB0` / `0x00425233`）: thiscallで ecx = MainMenu・スタックの引数4つ・`ret 0x10`。
type PortraitFnPtr = unsafe extern "thiscall" fn(u32, u32, u32, u32, u32);

struct RealTickIo {
    feed: FeedFn,
    local: LocalFn,
    /// 素のpoll（`POLL_FN`。照合が合わなければ`None`。そのときローカル入力は`local`で読む）。
    poll: Option<PollFn>,
    /// InputManager[0]〜[2]のアドレス（`GameAddrs::input_managers`）。pollが書く範囲をコピーして戻す先（サイドsのpollは[s]を書く）。
    ims: [u32; 3],
    set_state: u32,
    portrait: [u32; 3],
}

/// 素のpollのサイド`seat`（0〜2）を1回呼び下位16ビット（ax）を返す。
/// サイド0は1Pの設定どおりに読んだ入力ワード（ネットワークの対戦の自分の入力）。
/// サイド1は2Pの設定どおりに読んだ入力ワード。
/// サイド2は`poll_all`の最後の呼び（画面6など`[2]`を読む画面の入力ワード）。
/// InputManager[サイド]の`+0x00`〜`+0x29`（`IM_BYTES`バイト）は呼ぶ前にコピーし、呼んだ直後に書き戻す。
/// pollは入力ワードを返すだけでなく、今の入力ワード`+0x00`・前の入力ワード`+0x02`・縁`+0x06` / `+0x08`・リピートのカウンタ`+0x0A`〜`+0x29`を書く。
/// 書きっぱなしだと続く`feed(席, w)`が「前の入力ワード」を取り違え、押した瞬間とリピートが両者で食い違う。
///
/// # Safety
/// `poll`は素のpoll `0x0042B850`の入口（`POLL_SITES`の照合が合ったもの）か、同じ約束の検査の関数であること。約束: ecx = 席（0〜2。
/// `poll_all` `0x0042E9C0`も0・1・2で呼ぶ）、スタックの引数なし、素の`ret`、戻り値ax、ebx・esi・edi・ebpを保つ（`0x0042B85E`〜
/// `0x0042B860`でpush、`0x0042BE29`〜`0x0042BE31`でpop）。書く先はInputManager[席]（`0x4ACE18 + 席 * 0x8E`）の`+0x00`〜`+0x29`と
/// スタックだけ。`im`は同じ席のInputManagerの先頭で、`IM_BYTES`バイト読み書きできること。ゲームのスレッドから呼ぶこと。
unsafe fn poll_seat_keeping_im(poll: PollFn, seat: u32, im: *mut u8) -> u32 {
    let mut saved = [0u8; IM_BYTES];
    // SAFETY: `im`は`IM_BYTES`バイト読み取り可能（呼び出し側の約束）。先はスタックの配列で重ならない。
    unsafe { core::ptr::copy_nonoverlapping(im, saved.as_mut_ptr(), IM_BYTES) };
    // SAFETY: `poll`の約束は上のSafetyの節。
    let ax = unsafe { poll(seat) };
    // SAFETY: コピーの逆向き。`im`は`IM_BYTES`バイト書き込み可能。
    // このフィールドを書くのはゲームのスレッドだけ（feedとpoll_allも同じスレッド）なので読み手が途中の値を見ることはない。
    unsafe { core::ptr::copy_nonoverlapping(saved.as_ptr(), im, IM_BYTES) };
    // 上位16ビットは不定（`0x0042BE2A mov %si,%ax`はaxだけを書く）。
    ax & 0xFFFF
}

/// 1Pの入力機器（`INPUT_DEVICE_P1`から読んだ4バイトの下位1バイトが機器）がパッド（0 = PAD1・1 = PAD2）かどうか。
/// パッドのときだけ自分の入力にキーボードを足す。
/// 読み取れなかった（`None`）ときは足さない。
fn keyboard_joins_pad(device_word: Option<u32>) -> bool {
    device_word.is_some_and(|w| w & 0xFF < 2)
}

/// 1Pの入力機器を含む4バイト（`INPUT_DEVICE_P1`）。読み取れなければ`None`。例外を出さない読み方（`ReadProcessMemory`）で読み、確保もロック待ちもしない。
fn input_device_word() -> Option<u32> {
    #[cfg(test)]
    {
        fake_mem::input_device()
    }
    #[cfg(not(test))]
    {
        OwnProcess.u32_at(crate::th09_addr::INPUT_DEVICE_P1)
    }
}

/// 検査だけ: ゲームのメモリの代わりの表（アドレス → 値）。有効にしている間`RealTickIo`のゲームのメモリの読み取りはこの表を参照する（無いアドレスは0）。
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

    static DEVICE: AtomicU32 = AtomicU32::new(u32::MAX);

    /// 1Pの入力機器の偽の読み取り。設定していなければ読み取れなかったことにする（検査のプロセスの`0x004B353F`には別のものが入っているため）。
    pub fn input_device() -> Option<u32> {
        match DEVICE.load(Ordering::SeqCst) {
            u32::MAX => None,
            d => Some(d),
        }
    }

    pub fn set_input_device(device: Option<u32>) {
        DEVICE.store(device.unwrap_or(u32::MAX), Ordering::SeqCst);
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

    /// 無効なら`None`（本物の読みへ）。有効なら表の値（無ければ0）。
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
        // SAFETY: `addr`は`GameAddrs::direct_ranges`のアドレス（`arm`が読み取り可能と検証した。花映塚ではth09.exeの .dataでプロセスが終わるまでコミット済みで読み取り可能）。
        // `scripted::read_obs`はそれ以外を直接は読まない。
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
        // 検査: ゲームのInputManagerの本物のアドレスが渡されたら偽の3本から読む。
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
        // SAFETY: `addr`はInputManagerの3本のアドレス（`arm`が`IM_BYTES`バイト読み取り可能と検証した。花映塚では .data）。
        // 先はスタックの配列で重ならない。
        unsafe {
            core::ptr::copy_nonoverlapping(addr as usize as *const u8, out.as_mut_ptr(), IM_BYTES)
        };
    }
    fn local_input(&mut self) -> u32 {
        // 実際に操作する人の入力は1Pの設定どおりに読む（サイドが2Pでも同じ）。
        // 素のpollのサイド0をInputManager[0]を汚さずに呼ぶ。
        // pollの入口の照合が合わなかった実行（`POLL_FN` = 0）は今までの読み方へ戻る。
        if let Some(poll) = self.poll
            && self.ims[0] != 0
        {
            // SAFETY: `poll`は`POLL_SITES`の`fn_poll`行（`session::arm_poll`が実バイトと照合して一致したときだけ設定した入口）。
            // `ims[0]`は`arm_io`が設定したInputManager[0]。ゲームのスレッドから呼ぶ。
            let own = unsafe { poll_seat_keeping_im(poll, 0, self.ims[0] as usize as *mut u8) };
            // 1Pがパッドのとき、サイド0はキーボードの移動・ショットを読まない。
            // サイド2はFULLの配置のキーボードと両方のパッドを足したワードなので、足してキーボードでも動かせるようにする。
            // 2Pの入力機器もパッドならその分も混ざる。
            // 設定は毎tick読む（遊びながらキー設定を変えても効く）。読み取れなければサイド0だけ。
            if self.ims[2] != 0 && keyboard_joins_pad(input_device_word()) {
                // SAFETY: サイド0と同じ約束。サイド2は`poll_all`も呼ぶサイドで`ims[2]`は`arm_io`が設定したInputManager[2]。
                return own
                    | unsafe { poll_seat_keeping_im(poll, 2, self.ims[2] as usize as *mut u8) };
            }
            return own;
        }
        // SAFETY: `local`は`fn_local_input`行（照合で一致した入口）。引数なし・戻り値ax・ebxとediに触らずesiとebpを保ち、
        // 素の`ret`で戻るので、cdeclの引数なしの関数として呼んでよい。ゲームのスレッドから呼ぶ。
        unsafe { (self.local)() }
    }
    fn feed(&mut self, index: u32, word: u32) {
        // SAFETY: `feed`は`fn_feed`行（照合で一致した入口）。ecx = 番号（0〜2）、edx = 入力ワード、スタックの引数なし、esi・ediを保ち
        // ebx・ebpに触らないのでfastcallの2引数の関数として呼んでよい（fastcallとの対応は推定）。番号は`scripted::tick`が0・1・2だけを渡す。
        unsafe { (self.feed)(index, word) }
    }
}

impl NetIo for RealTickIo {
    fn poll_words(&mut self) -> Option<[u32; 3]> {
        let poll = self.poll?;
        if self.ims.contains(&0) {
            return None;
        }
        // SAFETY: `poll`と`ims`は`local_input`と同じ（サイド0の代わりに0・1・2。`poll_all` `0x0042E9C0`も同じ3つのサイドで呼ぶ）。
        // 3本とも`IM_BYTES`バイト読み取り可能と検証してある。ゲームのスレッドから呼ぶ。
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
        // SAFETY: `addr`は`net_game::direct_ranges`のアドレス（`arm_net`が読み取り可能と検証したth09.exeの .data）。
        // 検証は読み取りだけ。書き込めるのは、.dataが起動から終わりまでコミット済みで読み書きできるページのため。
        // `NetGame`はそれ以外へは直接書かない。
        // 書き込むのはゲームのスレッド（ゲーム自身がそのフィールドを書くのと同じスレッド）から。
        unsafe { (addr as usize as *mut u32).write_unaligned(v) }
    }
    fn safe_write_u8(&mut self, addr: u32, v: u8) -> bool {
        let mut n: usize = 0;
        // SAFETY: 自分のプロセスの擬似ハンドル・書き込み先のアドレス・書き込み元・長さ・書き込めたバイト数の格納先を渡すだけ。
        // アドレスが確保されていないか書き込めないときはOSが失敗を返す。
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
        // SAFETY: `set_state`は`fn_set_state`行（照合で一致した入口）。
        // thiscallで ecx = 乱数のアドレス`RNG_ADDR`・スタックの引数1つ（u16を32ビットに広げて積む）・`ret $0x4`で引数のスタックを自分で取り除く（ゲーム自身の呼び出し`0x00420A41`〜`0x00420A47`も`push %edx`）。
        // 関数のアドレスと整数は32ビットで同じ大きさ。
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
        // SAFETY: `[addr, addr + out.len())`は`net_game::direct_ranges`の範囲の中（`arm_net`が読み取り可能と検証した .data）。
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
        // SAFETY: `a`は`PORTRAIT_FNS`の1本（`session::arm_portrait`が実バイトと照合して合ったときだけ設定した）。
        // thiscallで ecx = MainMenu `[0x4AC8C0]`（`PortraitGate::ready`が0でないことと`[m+0x11B98]`（部品の配列）が0でないことを検証した直後）・スタックの引数4つ・`ret $0x10`で引数のスタックを自分で取り除く（`0x00424FAD` / `0x0042505C` / `0x0042538D`）。
        // ebx・esi・edi・ebpは保存して戻る。
        // ゲームのスレッド（ゲーム自身がMainMenuのハンドラでこの3本を呼ぶのと同じスレッド）から呼ぶ。
        // 引数は`net_portrait::plan`が作った4つ。カーソル（0〜15に絞り済み）から表で取得したキャラと、ゲーム自身が渡すのと同じ定数。
        // 関数のアドレスと整数は32ビットで同じ大きさ。
        unsafe {
            let f = core::mem::transmute::<usize, PortraitFnPtr>(a as usize);
            f(m, a0, a1, a2, a3)
        }
    }
    fn push_se(&mut self, id: u32, pan: i32) -> bool {
        #[cfg(test)]
        if fake_mem::on() {
            return false;
        }
        crate::rollback_play_win::push_se_checked(id, pan)
    }
    fn end_show(&mut self, show: &higanr_core::end_pick::Show) {
        crate::end_outline_win::show(show);
    }
    fn end_note(&mut self, seg: u16, note: higanr_core::end_pick::Note, frame: u32) {
        crate::end_outline_win::note(seg, note, frame);
    }
    fn end_shadow(&mut self) -> bool {
        crate::end_outline_win::shadow()
    }
}

impl StateMem for RealTickIo {
    fn loading(&mut self) -> bool {
        // 読み込みのアドレスは`GameAddrs::loading`（`arm_io`が4バイト読み取り可能と検証した .data）。
        let a = ADDRS[2].load(Ordering::Relaxed);
        TickIo::data_u32(self, a) != 0
    }
    fn read_u32(&mut self, addr: u32) -> Option<u32> {
        OwnProcess.u32_at(addr)
    }
    fn write_u32(&mut self, addr: u32, v: u32) -> bool {
        // Playerはヒープなので直接は書かず、例外を出さない書き方で書く（アドレスが確保されていなければ失敗で返る）。
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

/// 入力の入口（`stub.rs`）が、`SCRIPT_ON`が0でないときにtickの番号`k`を渡して呼ぶ。
/// 戻り値は 0 = 元の飛び先へ・1 = poll_all を呼ばずに戻る・2 = 描画だけのフレーム（ネットワークの経路だけが返す）。
/// パニックを受けたら台本・ネットワーク・撮影を止めて0を返す（ゲームは元の入力で続く）。
pub extern "C" fn script_on_tick(k: u32) -> u32 {
    match catch_unwind(AssertUnwindSafe(|| tick_inner(k))) {
        Ok(r) => r,
        Err(_) => {
            SCRIPT_ON.store(0, Ordering::SeqCst);
            UI_ON.store(0, Ordering::SeqCst);
            net_panicked();
            // rollbackのモードのフックが開けていた確保の向け先と監視の処理を閉じる（ネットワークが止まった後はフックが動かないので開いたまま残さない）。
            crate::rollback_win::RB_REDIRECT.store(0, Ordering::SeqCst);
            crate::rollback_win::RB_WATCH.store(0, Ordering::SeqCst);
            shot_win::set_off();
            crate::shot_key_win::clear();
            // 保存の画面を飛ばすフラグを立てたままなら下ろす（入口が閉じると監視できない。残すと継続の開始から画面15へ来たときに飛ぶ）。
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
    // SAFETY: `f`と`l`は`arm`が設定した関数の入口のアドレス（0でないことを上で見た）。`p`は`arm_poll`が照合の後に設定したpollの入口で、
    // 0なら`None`。関数のアドレスと整数は32ビットで同じ大きさ。呼ぶときの約束は`RealTickIo`の各関数の注。
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
    let ret = tick_body(k);
    shot_tick(ret);
    ret
}

/// 自分のPCにだけ撮るスクリーンショット（`shot_key_win`）の、毎tickのキー読み取り。
/// 入力の入口の戻り値が0でないtickは、ツールが入力を占有している（台本・ネットワークのfeed、または描画だけのフレーム）。
/// 0のtickはゲーム自身のpollがPを読むので、ここでは何もしない。
/// 入力を占有しているかどうかの判断は、全部の経路（ネットワーク・rollback・メニュー・台本）がこの戻り値に集まるのでここ1か所にある。
/// 進め直しのtick（rollbackの進め直し・観戦者の早送り）は1フレームに何度も来るので読まない。
fn shot_tick(ret: u32) {
    if !crate::shot_key_win::armed() || crate::rollback_play_win::resim_active() {
        return;
    }
    crate::shot_key_win::on_tick(ret != 0, HAS_SCRIPT.load(Ordering::Relaxed) != 0);
}

fn tick_body(k: u32) -> u32 {
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
    // rollbackのモードのフックが走査を呼んでいるtickはフックが出した指図（feedする入力ワードか描画だけ）に従う。
    // 台本・ネットワーク・自動保存・状態の操作には触らない（フックが持っているコンテナと重なる —— `RB_HELD`）。
    if let Some(r) = rb_tick(&mut io) {
        return r;
    }
    if RB_HELD.load(Ordering::Acquire) != 0 {
        // フックがコンテナを持っているのに指図が無い（来ない見込み）。
        // 0を返すと本物のキーボードがrollbackのtickに入ってしまうので、1 = poll_all を呼ばずに戻る。
        // 数えて本ログに出す。
        RB_STRAY.fetch_add(1, Ordering::Relaxed);
        return 1;
    }
    let r = route(&mut io, k);
    state_tick(&mut io, k);
    r
}

/// rollbackのモードのフックが出した指図を取ってfeedする（指図が無ければ`None` —— 今までの経路）。
/// feedはローカル入力を読まない（自分の入力ワードはフックが1フレームに1回だけ読む）。
/// 進め直しのtickは台本の時計（`TICKS`）を進めない。
fn rb_tick(io: &mut RealTickIo) -> Option<u32> {
    if !crate::rollback_play_win::armed() {
        return None;
    }
    let r = crate::rollback_play_win::input_hook(&mut |p1, p2, w2| {
        for (i, w) in [p1, p2, w2].into_iter().enumerate() {
            io.feed(i as u32, u32::from(w));
        }
    })?;
    if !crate::rollback_play_win::resim_active() {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    Some(r)
}

/// 台本のマッチの状態の操作を書く（どの経路のtickでも入口へ戻る直前に1回）。台本のコンテナが空か分岐が閉じた後は何もしない。
fn state_tick(io: &mut RealTickIo, k: u32) {
    // このtickの中で台本が終わって分岐を閉じたならコンテナに触らない（閉じた後は読み取りのスレッドが`arm`で設定し直しうる）。
    // 1と読めたなら閉じるのはゲームのスレッド（ここ）だけなので、この関数の間は設定し直されない。
    if SCRIPT_ON.load(Ordering::Acquire) == 0 {
        return;
    }
    // SAFETY: `SCRIPT_ON`をAcquireで1と読んだので`arm`が設定したコンテナが見える。
    // ここはゲームのスレッドで`route`の中の借用はもう終わっている。
    let Some(engine) = (unsafe { (*ENGINE.0.get()).as_mut() }) else {
        return;
    };
    let slots = [
        STATE_SLOTS[0].load(Ordering::Relaxed),
        STATE_SLOTS[1].load(Ordering::Relaxed),
    ];
    // ネットワーク（lockstep）と巻き戻しの実験の実行では書かずに断る（`state_ops`の先頭の注）。
    let refuse = if NET_ON.load(Ordering::Acquire) != 0 {
        Some(Refuse::Net)
    } else if crate::rollback_win::armed() {
        Some(Refuse::Rollback)
    } else {
        None
    };
    state_ops::apply(engine, io, slots, k, &STATE_LOG, refuse);
    // 段階の`type=` / `bs=` / `enter=`: ルーム名の入力欄へ文字を書き込む（ウィンドウから打たれた文字と同じ入力欄。入力欄が閉じていれば入らない。OSの入力は注入しない）。
    // 次のtickにメニューが取る。
    if let Some(t) = engine.take_typed() {
        let n = t.unit_count();
        let pushed = crate::text_input_win::push_script((0..n).filter_map(|i| t.unit(i)));
        TYPED_PUSHED.fetch_add(pushed as u32, Ordering::Relaxed);
        TYPED_DROPPED.fetch_add(n.saturating_sub(pushed) as u32, Ordering::Relaxed);
    }
}

// ---- ネットワークの対戦の「リプレイを保存して終了」で保存の画面を飛ばす ----
//
// 次が全部揃ったとき、グローバル状態ワードの`CONTINUED_BIT`を立てる: ネットワーク（`NetPlay::take_save_quit`）が「保存して終了」で閉じたtickだった・自動保存がそのマッチで保存関数を呼んでいる（`replay_save_win::saved_this_seg`）・分岐`SAVE_SKIP_SITE`の9バイトが合う。
// ゲームは画面15の最初のフレームでそれを見てタイトルへ移る。
// 立てたフラグは、画面14 / 15を通った後のタイトルか次に場面2へ入ったtickに下ろす（入力の入口の始まりで毎tick見る）。
// ゲームのスレッドで読み書きし、確保しない・待たない。
// 判断は`higanr_core::save_skip`。
// 観戦者の経路も同じフラグを立てる。理由: 観戦者のゲームは対戦のホストの押下を辿って同じ「保存して終了」になり、保存の画面が出てしまうため（自動保存の成否に依らない）。

/// 監視の状態（`save_skip::Watch`の番号`watch_code`）。書き込むのも読むのもゲームのスレッドだけ（パニックを受ける処理も同じスレッド）。
static SKIP_WATCH: AtomicU32 = AtomicU32::new(0);
/// 分岐のアドレス（検査は自分の静的な配列へ向ける）。
static SKIP_SITE_AT: AtomicU32 = AtomicU32::new(SAVE_SKIP_SITE.addr);
/// カウンタ（本ログの「ネットワークの外」の1行。ゲームのスレッドが書き込み、読み取りのスレッドが読む）。
/// 数えるのは次の回数: 立てた・もう立っていた・断った（`SkipRefuse::ALL`の順）・下ろした（タイトル・次のマッチ）・下ろせずに次のtickへ回した。
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

/// フラグを立てる・下ろすのに使う自分のプロセスのメモリ。
/// 読み取りは例外を出さない読み方（分岐は`read_own_into`、ワードは`safe_u32`と同じ`ReadProcessMemory`）で、書き込みは`write_own_u32`。
/// 検査ではワードの読み書きを`fake_mem`の表へ向ける。
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

/// ネットワークが「保存して終了」で閉じたtickに1回（`net_tick`）。`saved`はこのマッチで自動保存が保存関数を呼んだかどうか。
fn save_skip_mark(saved: bool) -> Marked {
    let mut own = skip_own();
    // 書き込むワードのアドレス（`arm`が設定した`global_state`）を、照合する9バイトの`testb`に埋まったアドレスと結び付ける（合わなければ`mark`が書かずに断る）。
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

/// ネットワークの経路の1 tick（`net_tick`）: マッチ終了時のメニューの「リプレイを保存して終了」でネットワークが閉じたtickなら、このマッチの自動保存が保存関数を呼んだときだけフラグを立てる。
/// このtickの始まりで自動保存の`on_tick`が済んでいる（`route`）。
/// 配線を検査で検証できるように、この判断を関数に切り出した。
/// `saved_this_seg()`の代わりに`true`を渡すと自動保存の無い実行でも保存の画面を飛ばし、リプレイが1本も残らない。
fn save_quit_tick(np: &mut NetPlay) {
    if np.take_save_quit() {
        save_skip_mark(save_skip::may_skip(
            SkipSide::Player,
            crate::replay_save_win::saved_this_seg(),
        ));
    }
}

/// 接続が終わった後の戻り道（`net_ui::NetUi`の`return_flow`）が、終わりのメニューの「リプレイを保存して終了」で試合の場面を出るのを見たtick:
/// `save_quit_tick`と同じ判断でこの試合の自動保存が保存関数を呼んでいれば保存の画面を飛ばすフラグを立てる。
/// ネットワークの経路と、後始末した後のメニューだけの経路の両方から呼ぶ。
fn return_save_quit_tick(out: &UiOut) {
    if out.save_quit {
        save_skip_mark(save_skip::may_skip(
            SkipSide::Player,
            crate::replay_save_win::saved_this_seg(),
        ));
    }
}

/// 観戦者の経路の1 tick（`watch_tick`）: 観戦者のゲームが対戦のホストの押下を辿って「リプレイを保存して終了」でマッチを出たtickなら、保存の画面を飛ばすフラグを立てる。
/// 対戦と違って自動保存が呼んだかどうかに依らない（`save_skip::may_skip`）。
fn watch_save_quit_tick(wp: &mut WatchPlay) {
    if wp.take_save_quit() {
        save_skip_mark(save_skip::may_skip(
            SkipSide::Watcher,
            crate::replay_save_win::saved_this_seg(),
        ));
    }
}

/// 入力の入口の始まりで毎tick（`route`）。
/// 監視していなければアトミック変数を1つ読んで戻る。
/// 監視していれば今の場面と（メニューなら）MainMenuの画面を読み、フラグを下ろすtickなら下ろす。
fn save_skip_tick(io: &mut RealTickIo) {
    if watch_of(SKIP_WATCH.load(Ordering::Relaxed)) == Watch::Idle {
        return;
    }
    let a = load_addrs();
    let scene = TickIo::data_u32(io, a.supervisor.wrapping_add(SUP_SCENE_CUR));
    // MainMenuのポインタは解放の後も0に戻らないので、例外を出さない読み方でたどる（`scripted::read_obs`と同じ）。
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
        // 読み取れない・書き込めない（来ない見込み）。監視を残して次のtickにやり直す。
        SKIP_WATCH.store(watch_code(next), Ordering::Relaxed);
        SKIP_CLEAR_RETRY.fetch_add(1, Ordering::Relaxed);
    }
}

/// 入力の入口でパニックを受けたとき（`script_on_tick`）: 立てたフラグが残っていれば下ろす（以後は入口が開かず監視できない）。
fn save_skip_abandon() {
    if watch_of(SKIP_WATCH.swap(0, Ordering::Relaxed)) != Watch::Idle {
        let _ = save_skip::clear(&mut skip_own(), CONTINUED_BIT);
    }
}

/// 本ログの「ネットワークの外」の1行（何も起きていなければ空）。
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

/// 自分のプロセスへ4バイトを例外を出さない書き方で書き込む（アドレスが確保されていないか書き込めないならfalse。例外は出ない）。
fn write_own_u32(addr: u32, v: u32) -> bool {
    let bytes = v.to_le_bytes();
    let mut n: usize = 0;
    // SAFETY: 自分のプロセスの擬似ハンドル・書き込み先のアドレス・書き込み元・長さ・書き込めたバイト数の格納先を渡すだけ。
    // アドレスが確保されていないか書き込めないときはOSが失敗を返す。
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

/// 自動保存へ見せるもの（読むだけ。確保しない）。
/// 対戦のセッションならネットワークのセッション、観戦者のセッションなら観戦しているマッチの1P・2Pの名前（まだ受けていなければ`WaitingForNames` —— 名前の無いうちは保存しない）。
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

/// 入口の1 tickの経路（ネットワーク・巻き戻し・台本）を選んで進める。戻り値は入口への戻り値。
fn route(io: &mut RealTickIo, k: u32) -> u32 {
    // SAFETY: `SCRIPT_ON`をAcquireで1と読んだので（`tick_inner`）`arm`が設定したコンテナが見える。以後はゲームのスレッド（ここ）だけが触る。
    // 入口を呼ぶゲームのスレッドは1本（実機でスレッドID 1種類）。
    let engine = unsafe { (*ENGINE.0.get()).as_mut() };
    // 観戦者の早送りの余分な回（`RESIM_TICK`）は台本の時計を進めない（進め直しの回と同じ。`rb_tick`）。
    if !crate::rollback_play_win::resim_active() {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    // ネットワークの対戦の「保存して終了」で立てたフラグは、ゲームが使い終えたら下ろす。
    // ネットワークを手放した後（再接続）やネットワークの外の経路でも見るので、ネットワークの経路より前に呼ぶ。
    save_skip_tick(io);
    // マッチの終わりのリプレイの自動保存（有効でなければアトミック変数を1つ読むだけ）。
    // ネットワークが動いていれば、印とフォルダの名前のためにネットワークのセッションを見せる（読むだけ）。
    if crate::replay_save_win::armed() {
        let (np, watching) = if NET_ON.load(Ordering::Acquire) != 0 {
            // SAFETY: `NET_ON`をAcquireで1と読んだので`arm_net`が組み込んだセッションが見える。触るのはゲームのスレッド（ここ）だけ。
            // この参照は`on_tick`の中で終わる。
            save_view(unsafe { (*NET.0.get()).as_ref() })
        } else {
            (None, crate::replay_save::Watching::No)
        };
        crate::replay_save_win::on_tick(np, watching);
    }
    // 素の花映塚でのリプレイ再生の監視の処理（再生中でなければ場面の2ワードを読んで戻る。自動保存の設定には依らない）。
    crate::replay_watch_win::on_tick();
    // ゲーム内のメニューが頼んだ「ネットワークを開く仕事」が終わっていればセッションを組み込む。
    // **組み込んだtickもそのまま下のネットワークの経路へ渡す。**
    // 組み込んだtickだけゲーム自身のpollに任せると、決定キーを押したままのとき画面6・カーソル4の決定が内蔵ネット対戦へ漏れるため。
    // 組み込むのは`UI_ON`の実行で、ネットワークがまだ無い間だけ。
    if UI_ON.load(Ordering::Acquire) != 0 && NET_ON.load(Ordering::Acquire) == 0 {
        poll_pending();
    }
    if NET_ON.load(Ordering::Acquire) != 0 {
        // SAFETY: `NET_ON`をAcquireで1と読んだので`arm_net` / `arm_watch`が組み込んだセッションが見える。以後はゲームのスレッドだけが触る。
        match unsafe { (*NET.0.get()).as_mut() } {
            Some(Slot::Play(slot)) => {
                let (mut relink, mut spectate, mut lan) = (false, false, None);
                let r = net_tick(slot, engine, io, k, &mut relink, &mut spectate, &mut lan);
                if relink {
                    // 接続が終わってマッチの外へ戻った（ゲーム内のメニューの実行だけ）。
                    // このtickの戻り値はネットワークの経路の判断のままにして、ネットワークのセッションを手放す。
                    // 次のtickからはメニューだけの経路になる。
                    relink_teardown();
                }
                if spectate {
                    // 待っているだけの対戦のセッション中に「ルームで観戦」が選択された。セッションを手放す。続けて観戦者のネットワークを開く。
                    request_spectate();
                }
                if let Some(ask) = lan {
                    // 待っているだけの対戦のセッション中にLANの項目が選択された。運び手（LANで接続コードを相手とやり取りするスレッド）を作る。セッションは手放す。
                    request_lan(ask);
                }
                lan_watch_poll();
                return r;
            }
            Some(Slot::Watch(slot)) => {
                let (r, follow) = watch_tick(slot, engine, io, k);
                if follow.release {
                    // 観戦が終わってマッチの外へ戻り、ルームのスレッドが終わりの状態を書き終えたらセッションを手放す（メニューは元の項目へ）。
                    watch_teardown();
                }
                if follow.spectate {
                    request_spectate();
                }
                if let Some(ask) = follow.lan {
                    request_lan(ask);
                }
                lan_watch_poll();
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
    // 巻き戻しの実験が有効なら台本より先に手順を回す。
    // 書き戻した後の進め直しのtickは記録した入力ワードをfeedして1を返す。台本は進めない。
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
    // 自動保存が有効なら台本が終わっても分岐を閉じない。
    // 台本の後のマッチの終わりでも保存を呼べるようにするため。
    // 台本の後は0を返し、元の入力で進む。
    script_tick(engine, io, k, crate::replay_save_win::armed())
}

/// 台本の1 tick（両方のサイドを台本が押す）。`keep_open`なら台本が終わっても分岐を閉じない（ネットワークの経路）。
fn script_tick(engine: &mut Engine, io: &mut RealTickIo, k: u32, keep_open: bool) -> u32 {
    if !matches!(engine.status(), Status::Running { .. }) {
        return 0;
    }
    let stepped = scripted::advance(engine, io, &load_addrs());
    script_commit(io, k, &stepped, keep_open, true)
}

/// 進めた台本の結果（`scripted::advance`）をfeedして入口への戻り値にする。
/// `place_shot`は撮影の要求を出すかどうか。
/// 観戦者の経路は進めたtickに`game.script_step`で出し終えているのでfalse。
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

/// ネットワークの経路の1 tick（入力の地点の判断 → 入口への戻り値）。
/// `relink`は接続が終わってネットワークのセッションを手放してよくなったかどうか（ゲーム内のメニューの実行だけ。`NetPlay::relink_due`）。
/// 呼び手がセッションの借用を終えてから`relink_teardown`を呼ぶ。
fn net_tick(
    slot: &mut NetSlot,
    mut engine: Option<&mut Engine>,
    io: &mut RealTickIo,
    k: u32,
    relink: &mut bool,
    spectate: &mut bool,
    lan: &mut Option<LanAsk>,
) -> u32 {
    // SAFETY: 引数の無い呼び出し。
    let tid = unsafe { GetCurrentThreadId() };
    let _ = GAME_TID.compare_exchange(0, tid, Ordering::SeqCst, Ordering::SeqCst);
    let (action, step) = {
        let mut game = NetGame::new(io, engine.as_deref_mut(), load_addrs());
        // ゲーム内のメニュー（`code_side = game`の実行だけ）は画面6で入力を占有する。接続した後の画面7ではキャンセルのときに占有する。
        // ネットワークの定期処理（受信・セッションづくり・終了フラグ）は、入力を占有するtickも毎tick先に行う（`net_ui::net_input`）。
        let ui = game_ui();
        let menu_run = ui.is_some();
        let now = if menu_run { net_udp::now_unix() } else { 0 };
        let (a, out) = net_ui::net_input(ui, &mut game, &mut slot.np, &mut slot.net, now);
        // メニューが入力を占有したtick（戻り道）は`dispatch`を通らないので、マッチの終わりの選択の縁取りはここでも消す。
        slot.np.end_leave(&mut game);
        return_save_quit_tick(&out);
        *spectate = spectate_asked(&out);
        *lan = out.lan;
        if (out.action.is_some() || out.leave)
            && let Some(ui) = game_ui()
        {
            publish_ui_stats(ui);
        }
        // 再接続: ネットワークが終わった後だけ場面を読む（接続している間・offer / answerの実行は何も足さない —— マッチの中の遅延の増分0）。
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
        // 台本が終わった（終わりに相手へBYE。2回目からは何もしない）。
        // 台本が無い実行と`after_script = keep`の最後の段階の後は閉じない（人の入力で続く）。
        slot.np.close(r);
    }
    NET_STATUS.publish(&slot.np, slot.net.is_vpn());
    if let Some(s) = step {
        // lockstepのフレームで台本を1 tick進めた（自分のサイドの入力ワードに使った）。
        if s.shot != 0 {
            shot_win::place_request(s.shot, s.step, k);
        }
        publish(s.status);
    }
    if let Action::Played(r) = action {
        NET_FRAMES.push(&r, &slot.np.last_timing());
    }
    // 2（描画だけのフレーム）の出口が設定されていることは`arm_net`が検証した（入口は出口が0なら戻りアドレスを替えない）。
    match net_game::entry_return(action) {
        Some(ret) => ret,
        None => match engine {
            Some(e) => script_tick(e, io, k, true),
            None => 0,
        },
    }
}

/// 観戦者の経路のtickの後に呼び手（`route`）がすること。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WatchFollow {
    /// 観戦者のセッションを手放す（観戦が終わってマッチの外へ戻り、ルームのスレッドが最後の状態を書き終えた）。
    release: bool,
    /// 「ルームで観戦」が選択された（`UiOut::spectate_open`）。
    spectate: bool,
    /// LANの項目が選択された（`UiOut::lan`）。
    lan: Option<LanAsk>,
}

/// 観戦者の経路の1 tick（`net_tick`の観戦者バージョン）。
/// 台本が終わってもネットワークは閉じない。観戦者の台本はタイトルから7の画面までを進め、以後は記録の列がゲームを動かす。
/// 台本は**接続している間も1 tickに1回進む**（`WatchPlay`のEscの読みが進める）。
/// 観戦中の台本の入力ワードはゲームへ渡さず、ESCビットだけが観戦をやめる読みに効く。
/// 接続する前やタイトルなど観戦者のゲーム自身の入力で進むtickは、台本と撮影を`net_tick`と同じに進める。
///
/// ゲーム内のメニューを組み込んだ実行（「ルームで観戦」）は、観戦者のセッションでも次を続ける（`net_ui::watch_input`）。
/// 画面6・カーソル4の決定を奪う入口の判定、観戦者が接続を待つ間の項目、左上の行、接続した後の画面7までの自動の前進。
/// 続けないと決定が内蔵ネット対戦へ漏れ、入力の地点が呼ばれなくなる。
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
                follow.lan = out.lan;
                slot.wp.end_leave(&mut game);
                if out.action.is_some() {
                    publish_ui_stats(ui);
                }
                // 終わったセッションは、マッチの外（メニューの場面）へ戻ってルームのスレッドが終わりの状態を書き終えたら手放す。
                // それまでは場面を読まない（観戦中のtickに足すのはアトミック変数1つの読みだけ）。
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
    NET_STATUS.publish_watch(&slot.wp, slot.net.is_vpn());
    if let Some(s) = step {
        if s.shot != 0 {
            shot_win::place_request(s.shot, s.step, k);
        }
        publish(s.status);
    }
    // 観戦者の画面の撮影。台本の段階は画面6で終わる。
    // そのため7以降は、段階や画面が変わったときとマッチ中の定期的なタイミングで描画の入口へ撮影を頼む。
    // 頼むだけで、撮るのは描画の入口。
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

/// 観戦者の経路の戻り値。`peeked`は接続している間のEscの読みが進めた台本の結果。
/// `Local`以外（記録の列がゲームを動かすtick）は、台本が進んでもその入力ワードをゲームへ渡さない。
/// `Local`は、読みで進めていればその結果をfeedする（二重に進めない）。
/// 進めていなければ（接続する前・終わった後）ここで台本を進める。
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

/// 開始のフック（`stub::match_start_entry`）が`NET_ON`が0でないときに呼ぶ。
/// パニックは受け止めて外へ出さない。受け止めたらネットワークを止める（入力の経路は台本だけで続く）。
pub extern "C" fn match_start_on() {
    #[cfg(test)]
    MATCH_ON_CALLS.fetch_add(1, Ordering::SeqCst);
    let r = catch_unwind(AssertUnwindSafe(|| {
        if NET_ON.load(Ordering::Acquire) == 0 {
            return;
        }
        if RB_HELD.load(Ordering::Acquire) != 0 {
            // rollbackのモードのフックがネットワークのセッションを持っている（走査の中から来た）。触らない（来ない見込み）。
            MATCH_NESTED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // SAFETY: 引数の無い呼び出し。
        let tid = unsafe { GetCurrentThreadId() };
        if GAME_TID.load(Ordering::SeqCst) != tid {
            // 入力の入口を通ったゲームのスレッドでない（か、まだ1度も入力の入口を通っていない）。触らない。
            MATCH_FOREIGN.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(mut io) = real_io() else {
            return;
        };
        // SAFETY: `NET_ON`をAcquireで1と読み、ここはゲームのスレッド（`GAME_TID`と同じ）。
        // 入力の入口の中から呼ばれることは無い（開始のフックはGameManagerの更新の中、入力の入口はSupervisorのon_tickの始まり）。
        match unsafe { (*NET.0.get()).as_mut() } {
            Some(Slot::Play(slot)) => {
                let mut game = NetGame::new(&mut io, None, load_addrs());
                slot.np.on_match_start(&mut game);
                NET_STATUS.publish(&slot.np, slot.net.is_vpn());
            }
            Some(Slot::Watch(slot)) => {
                let mut game = NetGame::new(&mut io, None, load_addrs());
                slot.wp.on_match_start(&mut game);
                WATCH_STATUS.publish(&slot.wp);
                NET_STATUS.publish_watch(&slot.wp, slot.net.is_vpn());
            }
            None => {}
        }
    }));
    if r.is_err() {
        net_panicked();
    }
}

// ---- rollbackのモードのフック（走査の包み・Presentの後）----

/// フックがネットワークのセッションと台本のコンテナを持っている間`RB_HELD`を1にする（入れ子の入力の入口と開始のフックがコンテナに触らないためのフラグ）。
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

/// ネットワークのセッション（対戦）を借りる。
/// 借りられるのはゲームのスレッドから、rollbackの入口を書き込めた実行のときだけ。
/// 観戦者のセッションの実行とネットワークが動いていない実行は`None`。
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
    // SAFETY: `NET_ON`をAcquireで1と読んだので`arm_net`が組み込んだセッションが見える。触るのはゲームのスレッドだけ。
    // このフックは走査の包みとPresentの後から順に呼ばれ、入れ子にならない。
    // 走査の中から来る入力の入口と開始のフックは、フックが持っている間（`RB_HELD`）はコンテナに触らない。
    match unsafe { (*NET.0.get()).as_mut() } {
        Some(Slot::Play(s)) => Some(&mut **s),
        Some(Slot::Watch(_)) | None => None,
    }
}

/// 先頭のフックが触ってよいネットワークのセッション（対戦）。
/// ネットワークが動いていない・観戦者・rollbackのフックがコンテナを持っている間・ゲームのスレッドでないときは`None`。
/// 検査がコンテナに触る条件だけを見られるよう`head_wait_hook`から分けてある。
fn head_wait_slot() -> Option<&'static mut NetSlot> {
    if NET_ON.load(Ordering::Acquire) == 0 || RB_HELD.load(Ordering::Acquire) != 0 {
        return None;
    }
    // SAFETY: 引数の無い呼び出し。
    let tid = unsafe { GetCurrentThreadId() };
    if GAME_TID.load(Ordering::SeqCst) != tid {
        return None;
    }
    // SAFETY: `NET_ON`をAcquireで1と読んだので`arm_net`が組み込んだセッションが見える。触るのはゲームのスレッドだけ。
    // 先頭のフックはdo_frameの最初（入力の入口・走査の包み・開始のフックの外）なので、どの借用とも入れ子にならない。
    match unsafe { (*NET.0.get()).as_mut() } {
        Some(Slot::Play(s)) => Some(&mut **s),
        Some(Slot::Watch(_)) | None => None,
    }
}

/// フレームの先頭のフック（`pacing_win::set_head_wait`）。
/// delay方式のlockstepのとき、自分の入力を読む前にこのtickで要る相手の入力ワードが届くまで待つ（`NetPlay::head_wait`）。
/// `pace_head`（do_frameの先頭`0x0042DC44`の入口）が呼ぶのは次の場合だけ。
/// 格子の待ちの後と、フルスクリーンのゲーム自身の時計の経路でゲームがその回にフレームを始める見込みのとき。
pub fn head_wait_hook() {
    if let Some(slot) = head_wait_slot() {
        let _ = slot.np.head_wait(&mut slot.net);
    }
}

/// 先頭のフックを設定する（ネットワークのセッションを動かす`NET_ON = 1`の前後。設定しても`NET_ON`が0の間は`head_wait_hook`が何もしない）。
fn install_head_wait() {
    crate::pacing_win::set_head_wait(Some(head_wait_hook));
}

/// 観戦者のセッションを借りる（走査の包みのフックから）。
/// 観戦者のセッションが無い・ネットワークが動いていない・ゲームのスレッドでないときは`None`。
/// 借りたらセッションを使い終えてから走査を呼ぶ（走査の中の入力の入口`watch_tick`が同じセッションを借りる）。
fn watch_slot_for_scan() -> Option<&'static mut WatchNetSlot> {
    if NET_ON.load(Ordering::Acquire) == 0 {
        return None;
    }
    // SAFETY: 引数の無い呼び出し。
    let tid = unsafe { GetCurrentThreadId() };
    if GAME_TID.load(Ordering::SeqCst) != tid {
        return None;
    }
    // SAFETY: `NET_ON`をAcquireで1と読んだので`arm_watch` / `install_slot`が組み込んだセッションが見える。触るのはゲームのスレッドだけ。
    // この参照は呼び手がコンテナを使い終えるまでしか持たず、走査を呼ぶ間は持たない。
    match unsafe { (*NET.0.get()).as_mut() } {
        Some(Slot::Watch(s)) => Some(&mut **s),
        Some(Slot::Play(_)) | None => None,
    }
}

/// 余分な走査の間だけ`RESIM_TICK`を立てる（`stub::input_wait_entry`が`_ticks.csv`と時刻の記録に積まないため。途中で失敗しても戻す）。
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
        // 余分な走査ごとに呼ぶのでsyscallの無い直読みバージョン（対戦の経路は変えない）。
        crate::rollback_win::scene_request_pending_fast()
    }
}

/// 観戦者のセッションがあるときの走査の包みのフック。使っていないマッチのフレームが溜まっている間は走査を余分に回す（`spectate_watch::run_display`）。
/// 走査のcallを書いていないときとセッションが無いときは`None`（普段の1回）。
/// 走査のcallはrollbackの設置か`arm_watch_scan`が書く。
/// rollbackの設置が済んでいること（`armed`）は要らない。早送りが使うのは走査のcall・文字の数のフィールド・`RESIM_TICK`だけ。
fn watch_scan(this: u32) -> Option<i32> {
    if !crate::rollback_win::scan_hooked() {
        return None;
    }
    watch_slot_for_scan()?;
    let mut host = RealFfHost { this, ascii0: None };
    Some(crate::spectate_watch::run_display(&mut host))
}

/// 走査の包み（`0x0042DCA9`）のフック（`rollback_win::set_scan_handler`）。
/// rollbackのモードのマッチの間は、ここで自分の入力ワードの取得・送信と入力の地点の処理を行う（止まっていれば書き戻しと進め直しも）。
/// そして更新1回か描画だけのフレームのために、走査を自分で呼ぶ。
/// `Some(ret)`を返すと包みはdo_frameへ`ret`を返す。
/// `None`なら走査を普段どおり1回呼ぶ（delay方式・場面の境目・マッチをやめた後・ネットワークの外）。
pub fn rb_scan_handler(this: u32) -> Option<i32> {
    let Some(slot) = rb_slot() else {
        // 対戦のセッションが無い。観戦者のセッションがあれば早送りの経路へ進む（遅れて入った観戦者を追いつかせる）。
        return watch_scan(this);
    };
    // 前のフレームの後半でマッチをやめていたらここでコピーする（やめた後は`active`がfalseになり、下のカウントのコピーが回らない）。
    rb_note_abort(&slot.np);
    let Some(mut io) = real_io() else {
        if slot.np.rollback_ready() {
            RB_BYPASS.fetch_add(1, Ordering::Relaxed);
        }
        return None;
    };
    // SAFETY: `rb_slot`が`NET_ON`を見て、ここはゲームのスレッド。台本のコンテナは`SCRIPT_ON`が1の間はゲームのスレッドだけが触る。
    // 走査の中の入力の入口は、フックが持っている間はコンテナに触らない（`RB_HELD`）。
    let mut engine = unsafe { (*ENGINE.0.get()).as_mut() };
    let _held = HeldGuard::new();
    let active = slot.np.rollback_ready();
    if active && crate::replay_save_win::armed() {
        // マッチの終わりのリプレイの自動保存。
        // delay方式では走査の中の入力の入口が毎tick呼ぶ。
        // rollbackのモードのtickは入口がコンテナに触れないので、フレームに1回ここで呼ぶ。
        crate::replay_save_win::on_tick(Some(&slot.np), crate::replay_save::Watching::No);
    }
    // 台本の撮影の要求を結び付けるtickの番号（フックが呼ぶ走査の入力の入口が取る番号 = 次の番号）。
    let k = stub::next_seq();
    let mut script_step = None;
    let out = crate::rollback_play_win::with_play(|st, mem| {
        let mut game = NetGame::new(&mut io, engine.as_deref_mut(), load_addrs());
        let r = slot
            .np
            .rollback_scan(&mut game, &mut slot.net, st, mem, &mut |rec, t| {
                NET_FRAMES.push(rec, t);
            });
        if matches!(r, netplay::RbScan::Ran { .. }) {
            // このフレームは入力の地点が`dispatch`を通らないので相手を待つ知らせと切断をここで進める。
            slot.np.peer_wait_tick(&mut game, &mut slot.net);
            // 同じく`NetUi::tick`も通らないので観戦コードのコピー（Ctrl+C）の読みと知らせもここで進める。
            if let Some(ui) = game_ui() {
                let (conn, _) = slot.np.ui_conn(0);
                let now = net_udp::now_unix();
                ui.copy_tick(&mut game, conn, now);
                // 新しいバージョンの知らせも`NetUi::tick`を通らないのでここで進める。
                ui.update_tick(&mut game, conn, now);
            }
        }
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
            // 台本が終わった（`net_tick`と同じ。2回目からは何もしない）。
            slot.np.close(r);
        }
        NET_STATUS.publish(&slot.np, slot.net.is_vpn());
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

/// Presentの後のフック（`pacing_win::set_after_present`）。rollbackのモードのマッチの間だけコピーする / 届いた入力ワードで確定の前線を進める。
/// 進め直しは更新を何度も走らせるので、x87の制御ワードをゲームの値にして行う（`rollback_play_win::with_game_fpu`）。
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

/// 走査の包みのフックとPresentの後のフックを設定する（`arm_rollback`が入口を全部書き込めた後に1回）。フックは`NET_ON`が1になるまで何もしない。
fn install_rollback_wiring() {
    crate::rollback_win::set_scan_handler(Some(rb_scan_handler));
    crate::pacing_win::set_after_present(Some(rb_post_present));
}

/// 観戦者のネットワークを開くとき、走査のcallだけを書いてフックを設定する（観戦者を追いつかせる早送り）。
/// 観戦者がどの方式のマッチを見るかは接続するまで分からない。
/// 早送りは方式に依らないので、マッチの方式に関わらず設定する。
/// **対戦の実行からは呼ばない**（delay方式の対戦の実行は1バイトも書かない約束）。
/// Presentの後のフックは設定しない（観戦者は巻き戻さない）。書いた（または既に書いてあった）らtrue。
pub fn arm_watch_scan(
    hook_env: &mut dyn crate::hook::HookEnv,
    forbidden: &[higanr_core::patch::AddrRange],
    out: &mut dyn std::io::Write,
) -> bool {
    let ok = crate::rollback_play_win::arm_watch_scan(hook_env, forbidden, out);
    if ok {
        // フックは走査のcallを書いた後に設定する（フックが先でも呼ばれるのはcallを書いた後）。
        // 観戦者のセッションが無い間は何もしない（`watch_scan`が`None`）。
        crate::rollback_win::set_scan_handler(Some(rb_scan_handler));
    }
    ok
}

/// 「ルームで観戦」で観戦者のネットワークを開く（開くスレッド）ときの`arm_watch_scan`。
/// 止めて書き込むのは`RealHook`。呼んだスレッド以外を止め、ゲームの60 Hzのスレッドが`0x0042DCA9`の命令の途中でない瞬間に、5バイトの命令のrel32を書き換えて戻す。
/// メニューの間の1回きりで、マッチの中ではない。
/// 本ログ向けの1行を返す。
fn arm_watch_scan_at_open() -> String {
    arm_watch_scan_at_open_with(&mut crate::hook_win::RealHook::new())
}

/// `arm_watch_scan_at_open`の本体（止めて書く環境を受ける。検査は偽の命令の表を渡す）。
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

/// DLLのフォルダの台本の名前。
pub const SCRIPT_FILE: &str = "script.txt";
/// 移した先の名前の末尾（本ログの名前の先頭に付ける）。
pub const SCRIPT_SUFFIX: &str = "_script.txt";

/// rollbackの設置でゲームのスレッドのIDをtickの記録から待つ上限（ms）。ゲームは起動時の読み込みの後に最初のtickを打つ。
/// 待つのは読み取りのスレッドだけで、ゲームは止まらない。見つからなければrollbackは設置せずdelay方式を名乗る。
const RB_TID_WAIT_MS: u64 = 15_000;

pub struct RealScriptHost {
    pub dll_dir: Option<PathBuf>,
    pub run_path: Option<PathBuf>,
    /// ゲームのフォルダ（exeのフォルダ）。ここと同じかその下のファイルには触らない。
    pub exe_dir: Option<PathBuf>,
}

impl RealScriptHost {
    fn guard(&self, dir: &Path) -> Result<(), String> {
        let exe = self
            .exe_dir
            .as_ref()
            .ok_or("exe のフォルダが不明なのでゲームのフォルダの外かチェックできません")?;
        if is_same_or_inside(&dir.to_string_lossy(), &exe.to_string_lossy()) {
            return Err(format!(
                "{} が exe のフォルダの中なので触りません",
                dir.display()
            ));
        }
        Ok(())
    }

    /// 台本の場所（ローダーの`--script`が環境変数で渡したパスか、DLLのフォルダの`script.txt`）。
    fn script_location(&self) -> Result<crate::net_config::Located, String> {
        let env = higanr_core::script::PATH_ENV;
        crate::net_config::locate(
            env,
            std::env::var_os(env).as_deref(),
            self.dll_dir.as_deref(),
            SCRIPT_FILE,
        )
    }

    /// 対戦のrollbackの設置（`ScriptHost::arm_rollback`の本体。観戦者の走査のcallはその呼び手が足す）。
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
        // 巻き戻しの実験（`[rollback_probe]`・`[alloc_count]`）が同じ入口を使うので、実験が有効な実行では設置しない。
        if let Ok(Some(text)) = self.read_net_ini() {
            let probe = !matches!(crate::rollback_probe::settings_from_text(&text), Ok(None));
            let count = !matches!(crate::alloc_count::settings_from_text(&text), Ok(None));
            if probe || count {
                return false;
            }
        }
        // ゲームのスレッドのIDをtickの記録から取る（確保の向け先を分けるため）。ゲームが動き出すのを待つ。
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
        // 被弾音・カットインの音の鳴らし方（iniの`rollback_hit_se`・`rollback_cutin_se`）。マッチの前に1回。
        let _ = crate::rollback_play_win::with_play(|st, _| {
            st.set_hit_se(s.rollback_hit_se);
            st.set_cutin_se(s.rollback_cutin_se);
        });
        // Presentの後のフックはペースの設置がPresentのcallを書いたときだけ届く。
        // 書き込めていなければ、コピーと進め直しは入力の地点（止まっている間）でしか起きない。
        // 正しさは変わらず、止まる回数と費用が増える。
        if crate::pacing_win::PRESENT_ORIGINAL.load(Ordering::Acquire) == 0 {
            let _ = writeln!(
                out,
                "巻き戻し（製品の処理フロー）: Present の call が書かれていない（[pacing] が入っていない）ので Present の後の手順は呼ばれません。進め直しは止まっている間の入力のフック地点だけになります。"
            );
        }
        install_rollback_wiring();
        true
    }

    /// iniの場所（ローダーの`--ini`が環境変数で渡したパスか、DLLのフォルダの`HiganRTool.ini`）。ネットワーク・ペース・自動保存・確保のカウントの全部がここを通る。
    fn ini_location(&self) -> Result<crate::net_config::Located, String> {
        let env = higanr_core::ini::PATH_ENV;
        crate::net_config::locate(
            env,
            std::env::var_os(env).as_deref(),
            self.dll_dir.as_deref(),
            crate::net_config::INI_FILE,
        )
    }

    /// 利用者へ配布するバージョンでiniが無ければ既定を書き出す（起動時に1回。`ini_seed`）。
    /// 場所が決まらないなら何もしない（読む側が同じ理由で断る）。
    pub(crate) fn seed_default_ini(&self, default: Option<&str>) -> crate::ini_seed::Seed {
        match self.ini_location() {
            Ok(loc) => crate::ini_seed::seed_process(&loc, default),
            Err(_) => crate::ini_seed::Seed::Skipped,
        }
    }

    fn source(&self) -> Result<PathBuf, String> {
        let loc = self.script_location()?;
        let dir = loc.path.parent().ok_or("台本のフォルダが不明")?;
        self.guard(dir)?;
        Ok(loc.path)
    }

    fn dest(&self) -> Result<(PathBuf, String), String> {
        let log = self.run_path.as_ref().ok_or("本ログの場所が未決定")?;
        let dir = log.parent().ok_or("本ログのフォルダが不明")?;
        self.guard(dir)?;
        let log_name = log
            .file_name()
            .ok_or("本ログの名前が不明")?
            .to_string_lossy();
        let name = side_file_name(&log_name, SCRIPT_SUFFIX);
        Ok((dir.join(&name), name))
    }
}

/// 場所のiniを読む。DLLのフォルダのiniが無ければ`Ok(None)`（ネットワークなしで走る）。
/// ローダーが渡したiniが無いのは渡し方の誤りなので、「無い ⇒ ネットワークなし」に丸めず、読めないとして返す。
/// `fallback`はDLLのフォルダにiniが無いときに返す中身（利用者へ配布するバージョンで書き出せなかったとき）。
#[cfg(test)]
pub(crate) fn read_ini_at(
    loc: &crate::net_config::Located,
    fallback: Option<&str>,
) -> Result<Option<String>, String> {
    Ok(read_ini_raw(loc, fallback)?.map(|(text, _)| text))
}

/// `read_ini_at`にShift_JISとして読み直したかどうか（UTF-8でなかったとき）を添える。
fn read_ini_raw(
    loc: &crate::net_config::Located,
    fallback: Option<&str>,
) -> Result<Option<(String, bool)>, String> {
    let path = &loc.path;
    match std::fs::read(path) {
        Ok(b) => decode_ini(b)
            .map(Some)
            .map_err(|why| format!("{} {why}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && loc.from_env.is_none() => {
            Ok(fallback.map(|t| (t.to_string(), false)))
        }
        Err(e) => Err(format!("{} 読み込み不能: {e}", path.display())),
    }
}

/// `read_ini_raw`に誤りの行を空行にする前処理（`ini_lenient::clean`）を掛けた結果。
/// Shift_JISとして読み直したときは先頭にその注を足す。
pub(crate) fn read_ini_cleaned(
    loc: &crate::net_config::Located,
    fallback: Option<&str>,
) -> Result<Option<higanr_core::ini_lenient::Cleaned>, String> {
    Ok(read_ini_raw(loc, fallback)?.map(|(text, sjis)| {
        let mut c = higanr_core::ini_lenient::clean(&text);
        if sjis {
            c.notes
                .insert(0, higanr_core::ini_lenient::IniNote::encoding());
        }
        c
    }))
}

/// UTF-8のiniはそのまま読む。そうでなければShift_JIS（CP932）として読む。どちらでもなければ断る。
fn decode_ini(bytes: Vec<u8>) -> Result<(String, bool), String> {
    match String::from_utf8(bytes) {
        Ok(s) => Ok((s, false)),
        Err(e) => cp932_to_string(e.as_bytes())
            .map(|s| (s, true))
            .ok_or_else(|| "が UTF-8 でも Shift_JIS でもありません".to_string()),
    }
}

fn cp932_to_string(bytes: &[u8]) -> Option<String> {
    use windows_sys::Win32::Globalization::{MB_ERR_INVALID_CHARS, MultiByteToWideChar};
    const CP932: u32 = 932;
    let len = i32::try_from(bytes.len()).ok()?;
    // SAFETY: `bytes`は`len`バイト読み取り可能。出力先はNULL・0（必要な長さだけを返す呼び方）。
    let n = unsafe {
        MultiByteToWideChar(
            CP932,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            len,
            core::ptr::null_mut(),
            0,
        )
    };
    if n <= 0 {
        return None;
    }
    let mut wide = vec![0u16; usize::try_from(n).ok()?];
    // SAFETY: 入力は上と同じ。出力先は`n`個のu16を書き込み可能。
    let wrote = unsafe {
        MultiByteToWideChar(
            CP932,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            len,
            wide.as_mut_ptr(),
            n,
        )
    };
    if wrote != n {
        return None;
    }
    String::from_utf16(&wide).ok()
}

fn wide(p: &Path) -> Vec<u16> {
    OsStr::new(p)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// `from`を`to`へ移す。`to`が既にあれば移さない（フラグ0は「上書きしない・別のドライブへはコピーしない」）。
pub(crate) fn move_no_replace(from: &Path, to: &Path) -> Result<(), u32> {
    let (f, t) = (wide(from), wide(to));
    // SAFETY: どちらも0で終わるUTF-16の列で呼び出しの間生きている。フラグは0。
    let ok = unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), 0) };
    if ok == 0 {
        // SAFETY: 引数の無い呼び出し。直前のMoveFileExWの結果を指す。
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
            Err(e) => Err(format!("{} 読み込み不能: {e}", src.display())),
        }
    }

    fn move_script(&mut self) -> Result<String, String> {
        // ローダーが渡した台本は移さない（`session::load_script`はこの関数を呼ばない。呼ばれたら断る）。
        if self.script_location()?.from_env.is_some() {
            return Err("ローダーが渡した台本は移動しません".to_string());
        }
        let src = self.source()?;
        let (to, name) = self.dest()?;
        move_no_replace(&src, &to).map_err(|code| {
            format!(
                "{} を {} へ移動不能（GetLastError = {code}）",
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
        Ok(self.read_net_ini_noted()?.map(|(text, _)| text))
    }

    fn read_net_ini_noted(
        &mut self,
    ) -> Result<Option<(String, Vec<higanr_core::ini_lenient::IniNote>)>, String> {
        // 読むだけ（台本と違って移さない）。
        let loc = self.ini_location()?;
        Ok(read_ini_cleaned(&loc, crate::ini_seed::fallback())?.map(|c| (c.text, c.notes)))
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
            // 場所が決まらないなら先に呼ばれる`read_script`が同じ理由で断っている（ここへは来ない）。
            Ok(_) | Err(_) => None,
        }
    }

    fn arm_net(&mut self, s: &NetSettings, set_state_fn: u32) -> Result<String, String> {
        // 接続コードのフォルダ（DLLのフォルダの`code\`かiniの`code_dir`）。ゲームのフォルダの中なら開かない。
        let code_dir = match s.code_side {
            None => None,
            Some(_) => {
                let d = net_code::code_dir(self.dll_dir.as_deref(), s.code_dir.as_deref())?;
                self.guard(&d)?;
                Some(d)
            }
        };
        // ゲーム内のメニューの実行は、ネットワークを開かずにメニューだけを組み込む。
        // 観戦用のコードのフォルダのチェック（下）は、この経路では要らない。
        // 観戦のホストは観戦者へルームで配信し、ファイルを使わないため。
        // そのチェックはこの分岐の後ろに書く。
        if s.code_side == Some(CodeSide::Game) {
            let ini_path = self.ini_location().ok().map(|l| l.path);
            return arm_game_ui(s, set_state_fn, code_dir, ini_path);
        }
        // 観戦用のコードのフォルダ（`[spectate] code_dir`、無ければ`[netplay]`の`code_dir`、無ければDLLのフォルダの`code`）。
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

    fn migrate_legacy(&mut self) -> Vec<String> {
        // 以前のバージョンが接続コードのフォルダに保存していた名前とルーム名をiniへ移す。
        // フォルダがゲームのフォルダの中なら触らない（`arm_net`と同じガード）。
        let Ok(loc) = self.ini_location() else {
            return Vec::new();
        };
        let setting =
            higanr_core::ini_store::get(&loc.path, crate::net_config::SECTION, "code_dir");
        let Ok(dir) = net_code::code_dir(self.dll_dir.as_deref(), setting.as_deref()) else {
            return Vec::new();
        };
        if self.guard(&dir).is_err() {
            return Vec::new();
        }
        crate::ini_save::migrate_legacy(&loc.path, &dir)
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
        // 観戦者の実行は、マッチの方式に関わらず走査のcallを書く（観戦者の早送り）。rollbackの設置が済んでいれば何もしない。
        // ゲーム内のメニューの実行が後から「ルームで観戦」を選ぶ場合は、ネットワークを開くとき（`open_watch_net`）に書く。
        // 起動時は観戦か対戦か分からず、対戦のdelay方式の実行には1バイトも書かないため。
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
        // ペースと時刻の記録（iniはネットワークと同じDLLのフォルダのHiganRTool.ini、CSVは本ログの隣）。
        Some(Box::new(crate::pacing_win::RealPacingHost::new(
            self.dll_dir.clone(),
            self.run_path.clone(),
            self.exe_dir.clone(),
        )))
    }

    fn overlay_host(&mut self) -> Option<Box<dyn crate::overlay::OverlayHost>> {
        // 文字の描画（`[overlay]`はネットワークと同じini。読むだけ）。サンプルのPNGは本ログの隣へ（ゲームのフォルダの中には書かない）。
        let ini = self.read_net_ini();
        crate::end_outline_win::configure(ini.as_ref().ok().and_then(|t| t.as_deref()));
        Some(Box::new(crate::overlay_win::RealOverlayHost::new(
            ini,
            self.run_path.clone(),
            self.exe_dir.clone(),
        )))
    }

    fn arm_replay(&mut self, net: Option<&NetSettings>) -> Result<Option<String>, String> {
        // リプレイ再生の監視の処理は自動保存の設定（`[replay]`）に依らず有効にする。有効にできない理由は本ログに残す（監視が無いと比べる材料が無い）。
        match crate::replay_watch_win::arm() {
            Ok(()) => crate::main_log::push_detail(
                "再生の見張り: 設置しました（再生したマッチの決着の値を1行ずつ書きます）",
            ),
            Err(why) => crate::main_log::push(format!("再生の見張り: 入れられません（{why}）。")),
        }
        // `[replay]`はネットワークと同じini（DLLのフォルダ）にある。読むだけ。
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
    //! 本物の入力の入口を検査のプロセスで通す。
    //! 使うのは偽のゲームのメモリ（静的変数）・偽のfeed・偽のローカル入力。
    //! 偽のfeedはfastcallで、模型どおりに偽のInputManagerを動かし、呼ばれた順を残す。
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

    /// 入力の入口の戻り値（0でなければツールが入力を占有した）と台本の有無から、P・Homeの押下を取るかどうかが決まる（`shot_tick`）。
    #[test]
    fn shot_press_is_taken_only_on_ticks_the_tool_holds_and_never_in_a_scripted_run() {
        use crate::pacing_win::PRESENT_ORIGINAL;
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let old = PRESENT_ORIGINAL.swap(1, Ordering::SeqCst);
        let tick = |ret: u32, down: Option<bool>| {
            crate::shot_key_win::test_set_down(down);
            FORCE_RET.store(ret + 1, Ordering::SeqCst);
            assert_eq!(script_on_tick(7), ret);
        };
        let press = |hold: u32| {
            tick(hold, Some(false));
            tick(hold, Some(true));
            crate::shot_key_win::test_pending()
        };

        // 台本の無い実行（arm_io）。
        prepare_entry(3);
        arm_io(fake_addrs(), feed_fn_addr(), local_fn_addr()).unwrap();
        SCRIPT_ON.store(1, Ordering::SeqCst);
        crate::shot_key_win::test_enable(true);
        assert!(!press(0), "ツールが握っていない tick（戻り値 0）で取った");
        assert!(press(1), "feed した tick（戻り値 1）で取れない");
        tick(0, Some(true));
        assert!(
            !crate::shot_key_win::test_pending(),
            "握っていない tick の後に持ち越しが残った"
        );
        assert!(press(2), "描画だけの tick（戻り値 2）で取れない");
        // 進め直しのtickは読まない。
        crate::shot_key_win::test_enable(true);
        crate::rollback_play_win::RESIM_TICK.store(1, Ordering::SeqCst);
        let in_resim = press(1);
        crate::rollback_play_win::RESIM_TICK.store(0, Ordering::SeqCst);
        assert!(!in_resim, "進め直しの回で読んだ");
        // 差し替え口を設定できなかった実行（PRESENT_ORIGINALが0）は読まない。
        crate::shot_key_win::test_enable(true);
        PRESENT_ORIGINAL.store(0, Ordering::SeqCst);
        assert!(!press(1), "Present の差し替え口が無いのに読んだ");
        PRESENT_ORIGINAL.store(1, Ordering::SeqCst);
        // 読んではいけない間（前面が別のアプリなど）は取らない。
        crate::shot_key_win::test_enable(true);
        tick(1, Some(false));
        tick(1, None);
        tick(1, Some(true));
        assert!(!crate::shot_key_win::test_pending());

        // 台本の付いた実行（arm）は、本物のキーを読まない。
        stop();
        prepare_entry(3);
        arm(
            parse("scene=3 wait=100 p1=Z\n").unwrap(),
            fake_addrs(),
            feed_fn_addr(),
            local_fn_addr(),
        )
        .unwrap();
        start().unwrap();
        crate::shot_key_win::test_enable(true);
        assert!(!press(1), "台本の実行で押下を取った");
        // 台本のコンテナを空にし直す（arm_io）とまた取る。
        stop();
        arm_io(fake_addrs(), feed_fn_addr(), local_fn_addr()).unwrap();
        SCRIPT_ON.store(1, Ordering::SeqCst);
        crate::shot_key_win::test_enable(true);
        assert!(press(1), "arm_io の後に取れない（台本の旗が残った）");

        crate::shot_key_win::test_enable(false);
        PRESENT_ORIGINAL.store(old, Ordering::SeqCst);
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
        // SAFETY: 分岐は閉じている（`prepare_entry`）。検査はREAL_HOOK_TESTSの下で1本ずつ。
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
        assert!(arm_net(s, 0x1234).unwrap_err().contains("既に開いています"));
        stop();
        let (s, _t) = slot();
        let e = arm_net_with(s, 0x1234, &[(0x10, 4)]).unwrap_err();
        assert!(
            e.contains("0x00000010") && e.contains("読み取り不能"),
            "{e}"
        );
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
        // SAFETY: 分岐は閉じている。検査はREAL_HOOK_TESTSの下で1本ずつ。
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
    /// 偽のpollが返す値（上位16ビットは捨てられるはず）。
    const POLL_VALUE: u32 = 0xDEAD_1234;

    /// 偽のpoll（ゲームのpollと同じfastcall・引数1つ）。
    /// 本物と同じくInputManager[サイド]の`IM_BYTES`バイトの全部（最後の1バイトまで）を、元の値と必ず違う値で書き散らす。
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
        // 1Pの入力機器ごとのワード。サイド 0 = 0x0011、サイド 1 = 0x0200（どの組み合わせでも混ざらない）、サイド 2 = 0x4400。
        // パッド（0・1）はサイド 0 | サイド2をサイド0・サイド2の順に2回。キーボードの配置（2〜4）と読めないときはサイド0の1回だけ。
        for (w, v) in POLL_WORDS.iter().zip([0xAB_0011u32, 0x0200, 0xCD_4400]) {
            w.store(v, Ordering::SeqCst);
        }
        let im_before = |i: usize| -> Vec<u8> { (0..IM_BYTES).map(|j| init(i, j)).collect() };
        for (i, im) in FAKE_IMS.iter().enumerate() {
            for (j, a) in im.iter().enumerate() {
                a.store(init(i, j), Ordering::SeqCst);
            }
        }
        for (device, want, polls, last_seat) in [
            (Some(0), 0x4411, 2, 2),
            (Some(1), 0x4411, 2, 2),
            (Some(2), 0x0011, 1, 0),
            (Some(3), 0x0011, 1, 0),
            (Some(4), 0x0011, 1, 0),
            (None, 0x0011, 1, 0),
            (Some(0xFFFF_FF02), 0x0011, 1, 0),
            (Some(0x0000_0100), 0x4411, 2, 2),
        ] {
            fake_mem::set_input_device(device);
            POLL_N.store(0, Ordering::SeqCst);
            assert_eq!(io.local_input(), want, "入力機器 {device:x?}");
            assert_eq!(POLL_N.load(Ordering::SeqCst), polls, "{device:x?}: 呼び数");
            assert_eq!(
                POLL_SEAT.load(Ordering::SeqCst),
                last_seat,
                "{device:x?}: 最後の席"
            );
            for i in 0..3 {
                assert_eq!(
                    dump(i),
                    im_before(i),
                    "{device:x?}: InputManager[{i}] が変わった"
                );
            }
        }
        fake_mem::set_input_device(None);
        for w in POLL_WORDS.iter() {
            w.store(POLL_VALUE, Ordering::SeqCst);
        }
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
        // SAFETY: 分岐は閉じている。検査はREAL_HOOK_TESTSの下で1本ずつ。
        unsafe { *NET.0.get() = None };
        SET_STATE_FN.store(0, Ordering::SeqCst);
        GAME_TID.store(0, Ordering::SeqCst);
        set_render_only_exit(0);
    }

    /// 画面6（Match）・カーソル`cursor`の偽のゲームを`fake_mem`に作る（場面1・次0、MainMenuのフィールド）。
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
        // SAFETY: 検査は`REAL_HOOK_TESTS`の下で1本ずつ。`UI_ON`が0の間に設定する（ゲームのスレッドは触らない）。
        unsafe { *UI.0.get() = Some(NetUi::new(ini, link, board)) };
        UI_ON.store(1, Ordering::Release);
        board
    }

    /// 入口を「メニューを組み込んだ実行」の形にする。
    /// 偽のfeed・ローカル入力・poll（サイドごとの入力ワード`words`。サイド0・1・2）・描画だけのフレームの出口を用意する。
    /// そして`SET_STATE_FN`を設定して分岐を開く。
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
        // SAFETY: 分岐は閉じている。検査はREAL_HOOK_TESTSの下で1本ずつ。
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

    const SPLIT_MENU: u32 = 0x2000_0000;

    fn fake_screen(screen: u32) {
        use crate::th09_addr::MENU_SCREEN_ID;
        fake_mem::set(SPLIT_MENU + MENU_SCREEN_ID, screen);
    }

    /// 場面1（メニュー）・画面6の偽のゲームで、台を分ける設定`on`のメニューを組み込んだ実行にする。
    fn arm_split_run(on: bool, words: [u32; 3]) -> &'static crate::net_ui::Board {
        use crate::th09_addr::APP_ACTIVE;
        crate::split_kb_win::TABLE.reset();
        fake_match_screen(3);
        arm_menu_run(words);
        fake_mem::set(APP_ACTIVE, 1);
        crate::digit_keys_win::test_set_foreground(true);
        let board: &'static crate::net_ui::Board = Box::leak(Box::new(crate::net_ui::Board::new()));
        let ini = Shape {
            mode: higanr_core::wire::Mode::Rollback,
            delay: 1,
        };
        // SAFETY: 検査は`REAL_HOOK_TESTS`の下で1本ずつ。ゲームのスレッドは居ない（検査のスレッドが入口を呼ぶ）。
        unsafe {
            *UI.0.get() =
                Some(NetUi::new(ini, Arc::new(UiLink::new()), board).with_split_keyboards(on));
        }
        board
    }

    fn press(dev: usize, vks: &[u8]) {
        crate::split_kb_win::TABLE.set_keys(dev, vks);
    }

    fn set_poll_words(words: [u32; 3]) {
        for (w, v) in POLL_WORDS.iter().zip(words) {
            w.store(v, Ordering::SeqCst);
        }
    }

    fn zeros() -> Vec<(u32, u32)> {
        vec![(0, 0), (1, 0), (2, 0)]
    }

    fn split_phase() -> higanr_core::split_kb::Phase {
        game_ui().map(|u| u.split_phase()).unwrap()
    }

    fn first_ask() -> higanr_core::split_kb::Phase {
        higanr_core::split_kb::Phase::Ask {
            stage: 0,
            p1: 0,
            p2: 0,
        }
    }

    /// 聞く7キーのVK（`split_kb::ASK_KEYS`と同じ順）。
    fn ask_vks() -> [u8; 7] {
        use higanr_core::split_kb::vk;
        [
            vk::Z,
            vk::X,
            vk::SHIFT,
            vk::UP,
            vk::DOWN,
            vk::LEFT,
            vk::RIGHT,
        ]
    }

    /// 台`dev`が`key`を押して離す。聞く間は0をfeedし続ける。
    fn tap(dev: usize, key: u8) {
        press(dev, &[key]);
        assert_eq!(hook_tick(), (zeros(), 0));
        press(dev, &[]);
    }

    /// 1Pの7キーを台`p1[i]`が、2Pの7キーを台`p2[i]`が順に押して離す。
    fn answer(p1: [usize; 7], p2: [usize; 7]) {
        for (dev, key) in p1.into_iter().chain(p2).zip(ask_vks().into_iter().cycle()) {
            tap(dev, key);
        }
    }

    /// 2台が押されたことがある状態で画面6から7へ移ったtickまで進める（案内の最初のtickのfeedと戻りを返す）。
    fn start_asking() -> (Vec<(u32, u32)>, u32) {
        use higanr_core::split_kb::vk;
        press(0, &[vk::Z]);
        press(1, &[vk::Z]);
        press(0, &[]);
        press(1, &[]);
        let (_, hits) = hook_tick();
        assert_eq!(hits, 0, "画面 6 はメニューが握る");
        fake_screen(7);
        hook_tick()
    }

    #[test]
    fn two_keyboards_are_split_into_1p_and_2p_through_the_real_hook() {
        use crate::net_ui::{SHOW_TL0, SHOW_TL1};
        use higanr_core::split_kb::{Phase, vk};
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _t = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        let board = arm_split_run(true, [0, 0, 0]);
        let (feeds, hits) = start_asking();
        assert_eq!((feeds, hits), (zeros(), 0), "案内の間は 0 を feed する");
        assert_eq!(split_phase(), first_ask());
        assert_eq!(board.show() & (SHOW_TL0 | SHOW_TL1), SHOW_TL0 | SHOW_TL1);
        // 台1が1Pの7キーを押す。
        for key in ask_vks() {
            tap(1, key);
        }
        assert_eq!(
            split_phase(),
            Phase::Ask {
                stage: 7,
                p1: 0b10,
                p2: 0
            }
        );
        // 2Pの段階で1Pの台が押しても進まない。
        tap(1, vk::Z);
        assert!(matches!(split_phase(), Phase::Ask { stage: 7, .. }));
        // 台0が2Pの7キーを押す。最後のキーは押したまま。
        for key in &ask_vks()[..6] {
            tap(0, *key);
        }
        press(0, &[vk::RIGHT]);
        assert_eq!(hook_tick(), (zeros(), 0));
        assert_eq!(split_phase(), Phase::Release { p1: 0b10, p2: 0b01 });
        assert_eq!(
            board.show() & (SHOW_TL0 | SHOW_TL1),
            SHOW_TL0,
            "離すのを待つ間は案内を 1 行だけ出す"
        );
        // 決定に使ったキーが離れるまで0。
        assert_eq!(hook_tick(), (zeros(), 0));
        press(0, &[]);
        assert_eq!(hook_tick(), (zeros(), 0));
        assert_eq!(split_phase(), Phase::Active { p1: 0b10, p2: 0b01 });
        assert_eq!(
            board.show() & (SHOW_TL0 | SHOW_TL1),
            0,
            "分けている間は案内を消す"
        );
        // 台ごとのワードを1P・2P・合成（1P|2P）としてfeedする。
        press(1, &[vk::LEFT, vk::Z]);
        press(0, &[vk::RIGHT, vk::X]);
        assert_eq!(
            hook_tick(),
            (vec![(0, 0x41), (1, 0x82), (2, 0xC3)], 0),
            "台 1 が 1P（左 + Z）、台 0 が 2P（右 + X）"
        );
        // ゲームが前面でなければ0（ゲームのpollへは任せない）。
        crate::digit_keys_win::test_set_foreground(false);
        assert_eq!(hook_tick(), (zeros(), 0));
        crate::digit_keys_win::test_set_foreground(true);
        fake_mem::set(crate::th09_addr::APP_ACTIVE, 0);
        assert_eq!(hook_tick(), (zeros(), 0));
        fake_mem::set(crate::th09_addr::APP_ACTIVE, 1);
        // 試合の場面でも続く。リプレイの再生になったら終わり。ゲーム自身の入力へ移る。
        fake_mem::set(
            crate::th09_addr::SUPERVISOR + crate::th09_addr::SUP_SCENE_CUR,
            2,
        );
        assert_eq!(hook_tick().0, vec![(0, 0x41), (1, 0x82), (2, 0xC3)]);
        fake_mem::set(crate::th09_addr::GLOBAL_STATE, 0x8);
        let (feeds, hits) = hook_tick();
        assert_eq!((feeds, hits), (vec![], 1), "リプレイでも分けた");
        assert_eq!(split_phase(), Phase::Idle);
        disarm_menu_run();
        crate::split_kb_win::TABLE.reset();
    }

    #[test]
    fn each_split_event_is_one_detail_line_and_a_single_keyboard_writes_none() {
        use higanr_core::split_kb::{Event, Phase, event_line, vk};
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _t = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let detail = |ev: Event| (true, event_line(ev, &crate::split_kb_win::TABLE));
        // 台が1台なら聞かず、行も出さない。
        crate::split_kb_win::test_take_lines();
        prepare_entry(3);
        arm_split_run(true, [0, 0, 0]);
        press(0, &[vk::Z]);
        press(0, &[]);
        hook_tick();
        fake_screen(7);
        hook_tick();
        assert_eq!(split_phase(), Phase::Idle);
        assert_eq!(
            crate::split_kb_win::test_take_lines(),
            vec![],
            "台が 1 台なのに行を書いた"
        );
        disarm_menu_run();
        // 聞き始め → 割り当て。イベントごとに詳細のログへ1行ずつ。
        prepare_entry(3);
        arm_split_run(true, [0, 0, 0]);
        start_asking();
        assert_eq!(
            crate::split_kb_win::test_take_lines(),
            vec![detail(Event::Asking { devices: 2 })]
        );
        answer([0; 7], [1; 7]);
        hook_tick();
        assert_eq!(split_phase(), Phase::Active { p1: 0b01, p2: 0b10 });
        assert_eq!(
            crate::split_kb_win::test_take_lines(),
            vec![detail(Event::Assigned { p1: 0b01, p2: 0b10 })]
        );
        // パッドのワードや割り当てていない台のキーが続いても分けたまま（戻さないので行も出さない）。
        set_poll_words([0x0001, 0, 0x0001]);
        press(2, &[vk::Z]);
        for _ in 0..300 {
            hook_tick();
        }
        assert_eq!(split_phase(), Phase::Active { p1: 0b01, p2: 0b10 });
        assert_eq!(crate::split_kb_win::test_take_lines(), vec![]);
        press(2, &[]);
        set_poll_words([0, 0, 0]);
        disarm_menu_run();
        // Escで分けない。
        prepare_entry(3);
        arm_split_run(true, [0, 0, 0]);
        start_asking();
        crate::split_kb_win::test_take_lines();
        press(0, &[vk::ESCAPE]);
        hook_tick();
        assert_eq!(
            crate::split_kb_win::test_take_lines(),
            vec![detail(Event::SkippedByEsc)]
        );
        press(0, &[]);
        hook_tick();
        assert_eq!(crate::split_kb_win::test_take_lines(), vec![]);
        disarm_menu_run();
        // パッドの決定で分けない（ゲームの入力があるのにどの台のキーも押されていない）。
        const Z: u32 = 0x0001;
        prepare_entry(3);
        arm_split_run(true, [Z, 0, Z]);
        start_asking();
        assert_eq!(
            crate::split_kb_win::test_take_lines(),
            vec![detail(Event::Asking { devices: 2 })]
        );
        for _ in 0..6 {
            hook_tick();
        }
        assert_eq!(
            crate::split_kb_win::test_take_lines(),
            vec![detail(Event::SkippedByPad)]
        );
        set_poll_words([0, 0, 0]);
        disarm_menu_run();
        crate::split_kb_win::TABLE.reset();
    }

    #[test]
    fn nothing_is_split_when_disabled_with_one_keyboard_or_for_cpu_battles() {
        use higanr_core::split_kb::{Phase, vk};
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _t = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        // iniで無効。
        arm_split_run(false, [0, 0, 0]);
        assert_eq!(start_asking(), (vec![], 1), "無効なのに握った");
        assert_eq!(split_phase(), Phase::Idle);
        disarm_menu_run();
        // 押されたことがある台が1台。
        prepare_entry(3);
        arm_split_run(true, [0, 0, 0]);
        press(0, &[vk::Z]);
        press(0, &[]);
        assert_eq!(hook_tick(), (zeros(), 0));
        fake_screen(7);
        assert_eq!(hook_tick(), (vec![], 1), "台が 1 台なのに握った");
        assert_eq!(split_phase(), Phase::Idle);
        disarm_menu_run();
        // 人間 対 人間でない選択（`[0x4A7EA4]`が0でない）。
        prepare_entry(3);
        arm_split_run(true, [0, 0, 0]);
        fake_mem::set(crate::th09_addr::SELECT_KIND, 1);
        assert_eq!(start_asking(), (vec![], 1), "CPU 戦で握った");
        assert_eq!(split_phase(), Phase::Idle);
        disarm_menu_run();
        // 難易度の画面へ画面6を経ずに入った（ストーリー・練習など）。
        prepare_entry(3);
        arm_split_run(true, [0, 0, 0]);
        press(0, &[vk::Z]);
        press(1, &[vk::Z]);
        fake_screen(7);
        assert_eq!(hook_tick(), (vec![], 1), "画面 6 を経ずに握った");
        disarm_menu_run();
        crate::split_kb_win::TABLE.reset();
    }

    #[test]
    fn esc_or_a_pad_press_while_asking_hands_the_input_back_to_the_game() {
        use higanr_core::split_kb::{Phase, vk};
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _t = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        arm_split_run(true, [0, 0, 0]);
        start_asking();
        // Escを押している間はゲームの入力もEscになる。ゲームへ渡すと難易度の画面が戻るので離すまで0をfeedする。
        const ESC: u32 = 0x0008;
        press(0, &[vk::ESCAPE]);
        set_poll_words([ESC, 0, ESC]);
        for _ in 0..4 {
            assert_eq!(hook_tick(), (zeros(), 0), "Esc がゲームへ漏れた");
            assert!(matches!(split_phase(), Phase::Drain { .. }));
        }
        press(0, &[]);
        assert_eq!(
            hook_tick(),
            (zeros(), 0),
            "ゲームの入力に Esc が残っている間"
        );
        set_poll_words([0, 0, 0]);
        assert_eq!(hook_tick(), (vec![], 1), "離れた tick から元の入力");
        assert_eq!(split_phase(), Phase::Skipped);
        press(1, &[vk::Z]);
        assert_eq!(hook_tick(), (vec![], 1), "使わない間に Z で割り当てた");
        disarm_menu_run();
        // パッドのボタン: ゲームの入力があるのにどの台のキーも押されていない状態が3 tick。決定のボタンを押している間は0をfeedする。
        const Z: u32 = 0x0001;
        prepare_entry(3);
        arm_split_run(true, [Z, 0, Z]);
        start_asking();
        assert_eq!(hook_tick(), (zeros(), 0));
        assert_eq!(hook_tick(), (zeros(), 0));
        assert_eq!(split_phase(), first_ask());
        for _ in 0..4 {
            assert_eq!(hook_tick(), (zeros(), 0), "パッドの決定がゲームへ漏れた");
        }
        assert!(matches!(split_phase(), Phase::Drain { .. }));
        set_poll_words([0, 0, 0]);
        assert_eq!(hook_tick(), (vec![], 1), "離れた tick から元の入力");
        assert_eq!(split_phase(), Phase::Skipped);
        disarm_menu_run();
        crate::split_kb_win::TABLE.reset();
    }

    #[test]
    fn pad_words_and_an_unassigned_keyboard_never_hand_the_input_back_while_split() {
        use higanr_core::split_kb::{Phase, vk};
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _t = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        arm_split_run(true, [0, 0, 0]);
        start_asking();
        answer([0; 7], [1; 7]);
        hook_tick();
        assert_eq!(split_phase(), Phase::Active { p1: 0b01, p2: 0b10 });
        // パッドのワード（ゲームの入力）が続いても割り当てた2台のワード（0）だけをfeedして分けたまま。ゲームのpollは読まない。
        POLL_N.store(0, Ordering::SeqCst);
        set_poll_words([0x0001, 0, 0x0001]);
        for _ in 0..300 {
            assert_eq!(hook_tick(), (zeros(), 0), "パッドの語で戻った");
        }
        assert_eq!(split_phase(), Phase::Active { p1: 0b01, p2: 0b10 });
        assert_eq!(
            POLL_N.load(Ordering::SeqCst),
            0,
            "分けている間に poll を読んだ"
        );
        // 割り当てていない台（3台目）のキーが押し続けられても戻らない。
        press(2, &[vk::Z]);
        for _ in 0..300 {
            assert_eq!(hook_tick(), (zeros(), 0), "3 台目の押下で戻った");
        }
        assert_eq!(split_phase(), Phase::Active { p1: 0b01, p2: 0b10 });
        press(2, &[]);
        set_poll_words([0, 0, 0]);
        disarm_menu_run();
        crate::split_kb_win::TABLE.reset();
    }

    #[test]
    fn a_script_fakes_two_keyboards_and_its_own_keys_are_not_fed_while_split() {
        use crate::th09_addr::{SUP_SCENE_CUR, SUP_SCENE_REQ};
        use higanr_core::split_kb::Phase;
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _t = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        crate::split_kb_win::TABLE.reset();
        fake_match_screen(3);
        // 台本が読む画面のアドレス一式（`fake_addrs`の偽のアドレス）も同じ偽のゲームにそろえる。
        let a = fake_addrs();
        fake_mem::set(a.supervisor + SUP_SCENE_CUR, 1);
        fake_mem::set(a.supervisor + SUP_SCENE_REQ, 0);
        fake_mem::set(a.main_menu_ptr, SPLIT_MENU);
        // 1Pは台0の7キー。2PはZ・X・Shiftが台1、↑↓←→ が台2（1人が2台を使う）。
        let names = ["Z", "X", "SHIFT", "UP", "DOWN", "LEFT", "RIGHT"];
        let mut text = String::new();
        for (i, name) in names.iter().enumerate() {
            text += &format!(
                "scene=1 screen=7 wait=3 kbd=0:{name} note=p1_{i}\n\
                 scene=1 screen=7 wait=3 kbd=0:none note=p1_up{i}\n"
            );
        }
        for (i, name) in names.iter().enumerate() {
            let dev = if i < 3 { 1 } else { 2 };
            text += &format!(
                "scene=1 screen=7 wait=3 kbd={dev}:{name} note=p2_{i}\n\
                 scene=1 screen=7 wait=3 kbd={dev}:none note=p2_up{i}\n"
            );
        }
        text += "scene=1 screen=7 wait=3 kbd=0:LEFT,1:SHIFT,2:RIGHT p1=Z p2=X hold=40 note=move\n";
        let script = parse(&text).unwrap();
        arm(script, a, feed_fn_addr(), local_fn_addr()).expect("arm");
        arm_poll(fake_poll as *const () as usize as u32);
        SET_STATE_FN.store(0x1234, Ordering::SeqCst);
        let board: &'static crate::net_ui::Board = Box::leak(Box::new(crate::net_ui::Board::new()));
        let ini = Shape {
            mode: higanr_core::wire::Mode::Rollback,
            delay: 1,
        };
        // SAFETY: 検査は`REAL_HOOK_TESTS`の下で1本ずつ。`UI_ON`が0の間に設定する（ゲームのスレッドは触らない）。
        unsafe {
            *UI.0.get() =
                Some(NetUi::new(ini, Arc::new(UiLink::new()), board).with_split_keyboards(true));
        }
        UI_ON.store(1, Ordering::Release);
        start().expect("start");
        // 2台が押されたことがある状態にする（本物はWM_INPUT。台本の実行は登録しない）。
        press(0, &[0x5A]);
        press(1, &[0x5A]);
        press(0, &[]);
        press(1, &[]);
        hook_tick();
        fake_screen(7);
        let mut seen = Vec::new();
        for _ in 0..200 {
            let (feeds, hits) = hook_tick();
            assert_eq!(hits, 0, "台を分ける間に元の入力へ任せた: {feeds:?}");
            seen.push((feeds, split_phase()));
            if split_phase()
                == (Phase::Active {
                    p1: 0b01,
                    p2: 0b110,
                })
                && seen.last().is_some_and(|(f, _)| f.iter().any(|w| w.1 != 0))
            {
                break;
            }
        }
        let (last, phase) = seen.last().unwrap();
        assert_eq!(
            *phase,
            Phase::Active {
                p1: 0b01,
                p2: 0b110
            },
            "{seen:?}"
        );
        assert_eq!(*last, vec![(0, 0x40), (1, 0x84), (2, 0xC4)]);
        // 割り当ての間と分けた後の全部のfeedに台本自身のZ（0x01）・X（0x02）は一度も無い。
        for (feeds, _) in &seen {
            assert!(
                feeds.iter().all(|w| w.1 & 0x03 == 0),
                "台本の語が feed された: {feeds:?}"
            );
        }
        assert!(
            seen.iter().any(|(_, p)| matches!(
                p,
                Phase::Ask {
                    stage: 7,
                    p1: 0b001,
                    ..
                }
            )),
            "台本の偽の押下が台 0 を 1P にしなかった"
        );
        stop();
        disarm_menu_run();
        crate::split_kb_win::TABLE.reset();
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
    fn the_update_hooks_exist_only_when_the_loader_allowed_the_notice_in_a_dist_build() {
        assert!(update_hooks(true, 5).is_some());
        assert!(update_hooks(false, 5).is_none(), "ローダーが許していない");
        assert!(update_hooks(true, 0).is_none(), "開発用のビルド");
        assert!(update_hooks(false, 0).is_none());
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
                false,
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
        // SAFETY: 分岐は閉じている。検査はREAL_HOOK_TESTSの下で1本ずつ。
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
                ini_path: None,
                spent: Vec::new(),
                lan: None,
                spectate_clip: None,
            },
        )
        .expect("ネットワークの組を開けない");
        let port = local.expect("受信ソケット").port();
        np.close(higanr_core::wire::ByeReason::Cancelled);
        // 観戦のホストを用意して後始末の後の節に集計の行が残るかを見る。
        let host = spectate_host::prepare(&s, local.expect("受信ソケット")).expect("観戦のホスト");
        assert!(recv.install_spectate(host.sender));
        *CODE_WATCH.lock().unwrap() = Some(watch);
        *RECV.lock().unwrap() = Some(recv);
        OPEN_STATE.store(OPEN_BUSY, Ordering::SeqCst);
        *PENDING.lock().unwrap() = Some(Ok(Pending::Play(Box::new(NetSlot { np, net }))));
        /// 途中で失敗してもほかの本物の入口の検査へ静的な値（ネットワークのセッション・メニュー・郵便受け・ワーカーのフラグ）を残さない。
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
            past.contains(&format!("{} 回目の接続（片づけ済み", closed0 + 1))
                && past.contains("ネットワーク: 段階 終えた"),
            "{past}"
        );
        assert!(
            past.contains("CHECK のズレ"),
            "前のつながりのネットワークの行: {past}"
        );
        assert!(
            past.contains("\n観戦: 観戦者 0 人（"),
            "止めた後の観戦のホストの集計の行: {past}"
        );
        let (snap, other) = net_status();
        assert!(
            other.contains("回目の接続（片づけ済み"),
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
        // SAFETY: ゲームのスレッドは居ない（検査のスレッドが入口を呼ぶ）。REAL_HOOK_TESTSの下で1本ずつ。
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

    #[test]
    fn a_closed_link_section_keeps_the_stopped_host_tree_and_measure_rows() {
        assert_eq!(closed_spectate_text(None), "");
        let s = spectate_host::Snapshot {
            host: higanr_core::spectate::HostCounts {
                intros: 2,
                uplinks_measured: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        let t = closed_spectate_text(Some(&s));
        for want in [
            "\n観戦: 観戦者 0 人（",
            "\n観戦者のツリー: 中継役 0 人 / 中継役への振り分け 2（",
            "\n上り回線速度の測定: 観戦者へ測定依頼を送った 0（測定依頼の再送 0） / 速度の確定 3・測定不能 0",
        ] {
            assert!(t.contains(want), "{want}\n---\n{t}");
        }
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
                ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
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
        // SAFETY: ゲームのスレッドは居ない（検査のスレッドが入口を呼ぶ）。REAL_HOOK_TESTSの下で1本ずつ。
        match unsafe { (*NET.0.get()).as_mut() } {
            Some(Slot::Watch(w)) => f(&mut w.wp),
            _ => panic!("観戦者のセッションが置かれていない"),
        }
    }

    /// ワーカーのフラグと待ちを検査用に替え、失敗するときも元に戻す（ほかの本物の入口の検査へ静的な値を残さない）。
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
            // SAFETY: ゲームのスレッドは居ない（検査のスレッドが入口を呼ぶ）。REAL_HOOK_TESTSの下で1本ずつ。
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
        // SAFETY: 分岐は閉じている。検査はREAL_HOOK_TESTSの下で1本ずつ。
        unsafe { *NET.0.get() = None };
        NET_ON.store(0, Ordering::SeqCst);
        SCRIPT_ON.store(1, Ordering::SeqCst);
        let th = post_watch_box();
        hook_tick();
        assert!(matches!(
            // SAFETY: ゲームのスレッドは居ない（検査のスレッドが入口を呼ぶ）。REAL_HOOK_TESTSの下で1本ずつ。
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
        // SAFETY: 分岐は閉じている。検査はREAL_HOOK_TESTSの下で1本ずつ。
        unsafe { *NET.0.get() = None };
        NET_ON.store(0, Ordering::SeqCst);
        SCRIPT_ON.store(1, Ordering::SeqCst);
        let th = post_watch_box();
        hook_tick();
        assert!(matches!(
            // SAFETY: ゲームのスレッドは居ない（検査のスレッドが入口を呼ぶ）。REAL_HOOK_TESTSの下で1本ずつ。
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
                ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
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
        // SAFETY: 分岐は閉じている。検査はREAL_HOOK_TESTSの下で1本ずつ。
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
                ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
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
    fn a_panic_with_the_net_on_makes_the_receive_thread_send_a_bye() {
        use higanr_core::auth::{AuthKey, Secret};
        use higanr_core::candidate::Candidates;
        use higanr_core::pong::PongResponder;
        use higanr_core::seal::{DirKeys, Sender};
        use higanr_core::wire::{self, ByeReason, Message};
        use std::net::{SocketAddr, UdpSocket};
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        const SID: u32 = 0x0B7E_0001;
        let dest = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        dest.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let path = dest.local_addr().unwrap();
        let key = AuthKey::derive(&Secret([0x44; 16]), SID);
        let dir = DirKeys::from_raw([0x45; 32], [0x46; 32]);
        let (mut net, th) = net_udp::open_for_code(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let r = PongResponder::new(
            SID,
            key.clone(),
            dir.opener(),
            dir.sealer(Sender::Pong),
            Candidates::from_addrs(&[path]).unwrap(),
            path,
        );
        assert!(netplay::Net::pong_install(&mut net, &r));
        let prev = RECV.lock().unwrap_or_else(|e| e.into_inner()).replace(th);
        prepare_entry(3);
        arm(
            parse("scene=3 p1=Z\n").unwrap(),
            fake_addrs(),
            feed_fn_addr(),
            local_fn_addr(),
        )
        .unwrap();
        start().unwrap();
        NET_ON.store(1, Ordering::SeqCst);
        PANIC_NEXT.store(true, Ordering::SeqCst);
        assert_eq!(script_on_tick(5), 0);
        assert_eq!(NET_ON.load(Ordering::SeqCst), 0);
        let panicked = NET_PANICKED.swap(0, Ordering::SeqCst);
        let mut buf = [0u8; 2048];
        let got = dest.recv_from(&mut buf).ok().and_then(|(n, _)| {
            wire::decode(&key, Some(&dir.swapped().opener()), &buf[..n])
                .ok()
                .map(|p| p.msg)
        });
        let th = std::mem::replace(&mut *RECV.lock().unwrap_or_else(|e| e.into_inner()), prev);
        drop(th);
        drop(net);
        assert_eq!(panicked, 1);
        assert_eq!(got, Some(Message::Bye(ByeReason::Cancelled)));
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
                    "マッチをやめた 1 回。最初は「{}」（確定したフレーム 1259 の後）",
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
                (Action::Local, 0), // 台本が終わったLocalは 0 = 元の飛び先
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
        // SAFETY: 分岐は閉じている（`prepare_entry`の`stop`）。検査はREAL_HOOK_TESTSの下で1本ずつ。
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

    /// 検査用: 文字列をShift_JIS（CP932）のバイト列にする。表せない文字があればNone。
    fn to_cp932(text: &str) -> Option<Vec<u8>> {
        use windows_sys::Win32::Globalization::{WC_NO_BEST_FIT_CHARS, WideCharToMultiByte};
        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut out = vec![0u8; wide.len() * 2 + 4];
        let mut used_default = 0i32;
        // SAFETY: 入力は`wide.len()`個のu16。出力先は`out.len()`バイト書き込み可能。既定の文字はNULL（使われたかどうかだけ`used_default`で見る）。
        let n = unsafe {
            WideCharToMultiByte(
                932,
                WC_NO_BEST_FIT_CHARS,
                wide.as_ptr(),
                wide.len() as i32,
                out.as_mut_ptr(),
                out.len() as i32,
                core::ptr::null(),
                &mut used_default,
            )
        };
        if n <= 0 || used_default != 0 {
            return None;
        }
        out.truncate(n as usize);
        Some(out)
    }

    #[test]
    fn a_shift_jis_ini_is_read_again_as_shift_jis_and_noted_once() {
        use crate::net_config::Located;
        use higanr_core::ini_lenient::NoteKind;
        let d = temp_dir("sjis");
        let at = |name: &str| Located {
            path: d.join(name),
            from_env: None,
        };
        // 利用者へ配布するiniの「〜」（U+301C）はShift_JISに無い（全角チルダU+FF5Eが同じ区点）ので検査用に直してから符号化する。
        let dist = include_str!("../../../scripts/dist/HiganRTool.ini")
            .replace("\r\n", "\n")
            .replace('\u{301C}', "\u{FF5E}")
            .replace(
                "name = HiganRTool",
                "name = 日本語のなまえ ; 名前のコメント",
            );
        let bytes = to_cp932(&dist).expect("Shift_JIS にできる文字だけ");
        assert!(String::from_utf8(bytes.clone()).is_err(), "UTF-8 ではない");
        std::fs::write(d.join("sjis.ini"), &bytes).unwrap();
        let c = read_ini_cleaned(&at("sjis.ini"), None).unwrap().unwrap();
        assert_eq!(c.text, dist, "日本語のコメントと値が読み戻せる");
        assert_eq!(
            c.notes.iter().map(|n| n.kind).collect::<Vec<_>>(),
            vec![NoteKind::Encoding]
        );
        let s = crate::net_config::from_text(&c.text).unwrap().unwrap();
        assert_eq!(
            s.name,
            crate::net_config::DEFAULT_NAME,
            "日本語の名前は使えない文字なので既定"
        );
        assert_eq!(s.ini_notes, vec![], "ネットワークの設定側の注は無い");
        // UTF-8の同じ内容は読み直さず、注も無い。
        std::fs::write(d.join("utf8.ini"), dist.as_bytes()).unwrap();
        let c = read_ini_cleaned(&at("utf8.ini"), None).unwrap().unwrap();
        assert_eq!((c.text.as_str(), c.notes.len()), (dist.as_str(), 0));
        // どちらの文字コードでもないバイト列は断る。
        std::fs::write(d.join("bad.ini"), [0x80u8, 0x80, 0xFF]).unwrap();
        let e = read_ini_cleaned(&at("bad.ini"), None).unwrap_err();
        assert!(e.contains("bad.ini") && e.contains("Shift_JIS"), "{e}");
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
        assert!(e.contains("移動不能"), "{e}");
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
    fn a_decided_name_is_used_by_the_next_net() {
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
        assert_eq!(lazy.settings_now().name, "second");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn legacy_files_move_into_the_ini_from_the_code_dir_unless_it_is_in_the_game_folder() {
        let d = temp_dir("legacy_host");
        let ini = d.join("HiganRTool.ini");
        std::fs::write(&ini, "[netplay]\nenable = 1\n").unwrap();
        let code = d.join("code");
        std::fs::create_dir_all(&code).unwrap();
        std::fs::write(code.join("player_name.txt"), "saved-synth\n").unwrap();
        let host = |exe_dir: PathBuf| RealScriptHost {
            dll_dir: Some(d.clone()),
            run_path: Some(d.join("logs").join("dll_x.log")),
            exe_dir: Some(exe_dir),
        };
        // ゲームのフォルダの中のフォルダには触らない。
        assert!(host(d.clone()).migrate_legacy().is_empty());
        assert!(code.join("player_name.txt").exists());
        assert!(
            !std::fs::read_to_string(&ini)
                .unwrap()
                .contains("saved-synth")
        );
        assert_eq!(host(d.join("game")).migrate_legacy().len(), 1);
        assert!(!code.join("player_name.txt").exists());
        assert!(
            std::fs::read_to_string(&ini)
                .unwrap()
                .contains("name = saved-synth")
        );
        // iniのcode_dirのフォルダも見る。
        std::fs::write(&ini, "[netplay]\nenable = 1\ncode_dir = other\n").unwrap();
        std::fs::create_dir_all(d.join("other")).unwrap();
        std::fs::write(d.join("other").join("player_name.txt"), "other-synth\n").unwrap();
        assert_eq!(host(d.join("game")).migrate_legacy().len(), 1);
        assert!(
            std::fs::read_to_string(&ini)
                .unwrap()
                .contains("name = other-synth")
        );
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
        // SAFETY: 分岐は閉じている（`prepare_entry`の`stop`）。ゲームのスレッドは居ない。検査はREAL_HOOK_TESTSの下で1本ずつ。
        unsafe { *NET.0.get() = Some(Slot::Play(Box::new(NetSlot { np, net }))) };
        // SAFETY: 引数の無い呼び出し。
        let tid = unsafe { GetCurrentThreadId() };
        let lent = || watch_slot_for_scan().is_some();
        NET_ON.store(1, Ordering::SeqCst);
        GAME_TID.store(tid, Ordering::SeqCst);
        assert!(!lent(), "Play の入れ物を貸した");
        // Watchのコンテナ（対照）。
        let (wnet, _wth) =
            net_udp::open_for_code(std::net::SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let wp = WatchPlay::new(
            std::sync::Arc::new(WatchSlot::new()),
            WatchConfig {
                wait_budget_us: 16_000,
                margin_frames: WATCH_MARGIN_FRAMES,
                require_net_selection: false,
                ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
            },
        );
        let wslot = Box::new(WatchNetSlot {
            wp,
            net: wnet,
            shots: Default::default(),
        });
        // SAFETY: 同上。上の借用は残っていない（`lent`は真偽だけを返す）。
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
        // rollbackのときは対戦のどの形（offer・answer・game）でも書くことを期待する（対照: 観戦者のrollbackの実行は今までどおり）。
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
        crate::rollback_win::TEST_OUTPUT_RANGES_NG.store(true, Ordering::SeqCst);
        let mut f = FakeCode::new();
        let note = arm_watch_scan_at_open_with(&mut f);
        crate::rollback_win::TEST_OUTPUT_RANGES_NG.store(false, Ordering::SeqCst);
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
        // SAFETY: 分岐は閉じている（`prepare_entry`の`stop`）。ゲームのスレッドは居ない。検査はREAL_HOOK_TESTSの下で1本ずつ。
        unsafe { *NET.0.get() = Some(Slot::Play(Box::new(NetSlot { np, net }))) };
        // SAFETY: 引数の無い呼び出し。
        let tid = unsafe { GetCurrentThreadId() };
        let lent = || head_wait_slot().is_some();
        // 対照: 4つがそろえば対戦のネットワークのセッションを先頭のフックへ貸す。
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
        // SAFETY: 同上。上の借用は残っていない（`lent`は真偽だけを返す）。
        unsafe { *NET.0.get() = None };
        assert!(!lent(), "入れ物が無いのに貸した");
        NET_ON.store(0, Ordering::SeqCst);
        GAME_TID.store(0, Ordering::SeqCst);
    }

    /// 偽のメモリ（`fake_mem`）の上のグローバル状態ワード。アドレスは本物と同じにする。
    /// 立てる前に、照合した9バイトの`testb`に埋まったアドレスと書くワードのアドレスを結び付けるので、ずらすと断る。
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
                ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
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
    fn the_return_flows_save_quit_marks_only_when_the_autosave_ran() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let addrs_before = ADDRS[3].load(Ordering::SeqCst);
        ADDRS[3].store(FAKE_STATE, Ordering::SeqCst);
        fake_mem::enable();
        skip_reset();
        SKIP_SITE_AT.store(
            SAVE_SKIP_SITE.expected.as_ptr() as usize as u32,
            Ordering::SeqCst,
        );
        let word = || fake_mem::get(FAKE_STATE).unwrap();
        fake_mem::set(FAKE_STATE, 0x0001_0004);
        let asked = UiOut {
            save_quit: true,
            ..UiOut::default()
        };
        crate::replay_save_win::set_seg_saved_for_test(true, true);
        return_save_quit_tick(&UiOut::default());
        assert_eq!(word(), 0x0001_0004, "戻り道が見ていないのに立てた");
        crate::replay_save_win::set_seg_saved_for_test(true, false);
        return_save_quit_tick(&asked);
        assert_eq!(word(), 0x0001_0004, "自動保存が呼んでいない試合で立てた");
        crate::replay_save_win::set_seg_saved_for_test(true, true);
        return_save_quit_tick(&asked);
        assert_eq!(word(), 0x0001_0004 | CONTINUED_BIT);
        assert_eq!(watch_of(SKIP_WATCH.load(Ordering::SeqCst)), Watch::Leaving);

        crate::replay_save_win::set_seg_saved_for_test(false, false);
        skip_reset();
        fake_mem::clear();
        ADDRS[3].store(addrs_before, Ordering::SeqCst);
    }

    /// 組み上がったものの検査: ネットワークを後始末した後のメニューだけの経路（入口 → `ui_only_tick`）で、接続が終わった後の戻り道が試合の場面を止める。
    /// そして「保存して終了」の切替を見たtickに、保存の画面を飛ばすフラグを立てる。
    #[test]
    fn the_menu_only_path_freezes_the_match_and_marks_the_save_skip_for_the_return_flow() {
        use crate::th09_addr::{
            LOADING, MATCH_END_MENU, PAUSE, SUP_SCENE_CUR, SUP_SCENE_REQ, SUPERVISOR,
        };
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        fake_match_screen(0);
        arm_menu_run([0, 0, 0]);
        let board: &'static crate::net_ui::Board = Box::leak(Box::new(crate::net_ui::Board::new()));
        let ini = Shape {
            mode: higanr_core::wire::Mode::Rollback,
            delay: 1,
        };
        // SAFETY: 検査は`REAL_HOOK_TESTS`の下で1本ずつ。ゲームのスレッドの入口はこの後の`hook_tick`からしか来ない。
        unsafe {
            *UI.0.get() =
                Some(NetUi::new(ini, Arc::new(UiLink::new()), board).with_return_flow(None))
        };
        game_ui()
            .expect("メニュー")
            .on_lost(higanr_core::return_flow::Why::PeerLeft);
        for (a, v) in [
            (SUPERVISOR + SUP_SCENE_CUR, 2),
            (SUPERVISOR + SUP_SCENE_REQ, 2),
            (LOADING, 0),
            (PAUSE, 0),
            (MATCH_END_MENU, 0),
        ] {
            fake_mem::set(a, v);
        }
        for t in 0..higanr_core::return_flow::HOLD_TICKS {
            let (feeds, hits) = hook_tick();
            assert!(
                feeds.is_empty() && hits == 0,
                "tick {t}: 入力止めの間に feed した・poll へ任せた"
            );
            // 出口（描画だけのフレーム）を通るとeaxは3（`fake_exit`）。
            assert_eq!(
                load(&AFTER)[0],
                3,
                "tick {t}: 試合を描画だけのフレームで止めていない"
            );
        }
        // フラグを頼んだかどうかは、自動保存が呼んでいない試合の断りの数で見る（断りはメモリに触る前に出る）。
        // 立てる・照合する判断は`the_return_flows_save_quit_marks_only_when_the_autosave_ran`で見る。
        skip_reset();
        crate::replay_save_win::set_seg_saved_for_test(false, false);
        let not_saved = SkipRefuse::ALL
            .iter()
            .position(|&r| r == SkipRefuse::NotSaved)
            .unwrap();
        hook_tick();
        assert_eq!(
            SKIP_REFUSED[not_saved].load(Ordering::SeqCst),
            0,
            "切替の前に頼んだ"
        );
        fake_mem::set(SUPERVISOR + SUP_SCENE_REQ, netplay::REQ_SAVE_AND_QUIT);
        hook_tick();
        assert_eq!(
            SKIP_REFUSED[not_saved].load(Ordering::SeqCst),
            1,
            "戻り道の「保存して終了」で保存の画面を飛ばす印を頼んでいない"
        );

        skip_reset();
        disarm_menu_run();
    }

    /// 同じことを終わったネットワークのセッションが残っている経路（入口 → `net_tick` → `net_input`）で行う。
    #[test]
    fn the_net_path_also_marks_the_save_skip_for_the_return_flow() {
        use crate::th09_addr::{
            LOADING, MATCH_END_MENU, PAUSE, SUP_SCENE_CUR, SUP_SCENE_REQ, SUPERVISOR,
        };
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        prepare_entry(3);
        fake_match_screen(0);
        arm_menu_run([0, 0, 0]);
        let board: &'static crate::net_ui::Board = Box::leak(Box::new(crate::net_ui::Board::new()));
        let ini = Shape {
            mode: higanr_core::wire::Mode::Rollback,
            delay: 1,
        };
        // SAFETY: 検査は`REAL_HOOK_TESTS`の下で1本ずつ。ゲームのスレッドの入口はこの後の`hook_tick`からしか来ない。
        unsafe {
            *UI.0.get() =
                Some(NetUi::new(ini, Arc::new(UiLink::new()), board).with_return_flow(None))
        };
        OPEN_STATE.store(OPEN_BUSY, Ordering::SeqCst);
        *PENDING.lock().unwrap() = Some(Ok(pending_box()));
        hook_tick();
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            1,
            "検査の前提: セッションを置いた"
        );
        // SAFETY: 同上（入口の外。セッションに触るのはこの検査だけ）。
        match unsafe { (*NET.0.get()).as_mut() } {
            Some(Slot::Play(s)) => s.np.close(higanr_core::wire::ByeReason::Disconnected),
            _ => panic!("対戦者のセッションが無い"),
        }
        game_ui()
            .expect("メニュー")
            .on_lost(higanr_core::return_flow::Why::Lost);
        for (a, v) in [
            (SUPERVISOR + SUP_SCENE_CUR, 2),
            (SUPERVISOR + SUP_SCENE_REQ, 2),
            (LOADING, 0),
            (PAUSE, 0),
            (MATCH_END_MENU, 0),
        ] {
            fake_mem::set(a, v);
        }
        for _ in 0..higanr_core::return_flow::HOLD_TICKS {
            hook_tick();
        }
        skip_reset();
        crate::replay_save_win::set_seg_saved_for_test(false, false);
        let not_saved = SkipRefuse::ALL
            .iter()
            .position(|&r| r == SkipRefuse::NotSaved)
            .unwrap();
        fake_mem::set(SUPERVISOR + SUP_SCENE_REQ, netplay::REQ_SAVE_AND_QUIT);
        hook_tick();
        assert_eq!(
            NET_ON.load(Ordering::SeqCst),
            1,
            "検査の前提: ネットワークの経路のまま"
        );
        assert_eq!(
            SKIP_REFUSED[not_saved].load(Ordering::SeqCst),
            1,
            "ネットワークの経路で戻り道の「保存して終了」の印を頼んでいない"
        );

        skip_reset();
        disarm_menu_run();
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
        let typed = higanr_core::room_input::RoomBuf::from_name("Typed-Room").unwrap();
        link.send(Cmd::Spectate(typed));
        let (room, shown) = watch_target(&link).unwrap();
        assert_eq!(room, typed.parse().unwrap().spectate_of());
        assert_eq!(shown, "Typed-Room");
        // 一覧から選んだ行。ハッシュから観戦用のルーム名を戻し、左上の名は「1P vs 2P」になる。対戦のホストが割り当てた名から導く名と同じ。
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
        // 打ち直したら古い選択ではなく打った名になる。
        link.send(Cmd::Spectate(typed));
        let (room, _) = watch_target(&link).unwrap();
        assert_eq!(room, typed.parse().unwrap().spectate_of());
    }

    #[test]
    fn a_lan_line_reaches_the_simple_log_and_the_detail_list() {
        let line = "LAN: 検査の行（/8）を足した範囲で開きます。".to_string();
        lan_note(line.clone());
        assert!(crate::main_log::peek().contains(&line), "本ログに無い");
        assert!(
            GAME_NET_NOTES.lock().unwrap().contains(&line),
            "詳細の一覧に無い"
        );
    }

    #[test]
    fn an_open_line_reaches_the_simple_log_only_when_the_spectate_setup_failed() {
        let failed = [
            " / 観戦は使いません: 検査の理由 A",
            " / 観戦の配信を開始できませんでした（送り手の登録が重複）",
            " / 検査 C。ただし、コードを書けませんでした: 検査の理由",
        ];
        for (i, tail) in failed.iter().enumerate() {
            let line = format!("ネットワーク: 検査の開いた行 {i}（失敗）{tail}");
            game_net_open_note(line.clone());
            assert!(
                crate::main_log::peek_brief().contains(&line),
                "本ログに無い: {line}"
            );
            assert!(
                GAME_NET_NOTES.lock().unwrap().contains(&line),
                "詳細の一覧に無い: {line}"
            );
        }
        let fine = "ネットワーク: 検査の開いた行（成功） / 観戦の配信側を用意しました（観戦用の session 0x00000001）".to_string();
        game_net_open_note(fine.clone());
        assert!(
            !crate::main_log::peek_brief().contains(&fine),
            "成功の文が簡潔にも出た"
        );
        assert!(
            GAME_NET_NOTES.lock().unwrap().contains(&fine),
            "詳細の一覧に無い"
        );
    }
}
