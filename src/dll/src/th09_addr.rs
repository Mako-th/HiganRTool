//! 花映塚 1.50a（th09.exe）のアドレスと期待バイト列。**この値はここ 1 か所にだけ置く**（同定の値だけは `higanr_core::pe::TH09_V150A`）。
//! 各行の `source` に、解析のノートの節と逆アセンブルの行を書く。ノートに無い値は書かない
//! （転記ミスは `every_site_appears_in_the_note_with_its_address` が見る）。アドレスはすべて image base `0x400000` の VA（ASLR 無し）。
#![forbid(unsafe_code)]

use higanr_core::patch::AddrRange;

use crate::rollback_draw::{AliveRule, RbSlotArray};
use crate::rollback_play::RbRenderReset;
use crate::rollback_probe::{RbBlock, RbCallers, RbData, RbEffect, RbSkip, RbSkipKind, RbVmArray};
use crate::sites::{SiteDef, Use};

/// 地点の表の `source`（ノートの節と逆アセンブルの行）を置く。配布のビルド（feature `dist`）では `""` にする（配るバイナリに解析の跡を残さないため）。
/// `macro_rules!` の定義を 2 つに分けるのは、配布のビルドでは文字列が展開の段階で消え、成果物に残るかをコンパイラの作りに頼らずに済むため。
#[cfg(not(feature = "dist"))]
macro_rules! site_source {
    ($s:literal) => {
        $s
    };
}
#[cfg(feature = "dist")]
macro_rules! site_source {
    ($s:literal) => {
        ""
    };
}

/// th09.exe の image base。これと違う位置に読まれていたら、表の VA はどれも当たらない。
pub const IMAGE_BASE: u32 = 0x0040_0000;

/// 最初のフックで書く行の名前（`0x0043112D` の call 先の差し替え）。アドレスと期待バイト列はこの名前の行（下の表）から引き、ここには転記しない。
pub const HOOK_SITE: &str = "input_wait_call";

// ---- tick ごとの記録で入口が読むアドレスとオフセット ----
// ここの値は書かずに読むだけ。検査 `record_offsets_appear_in_the_design` が設計の本文と突き合わせる。

/// ウィンドウの構造体（do_frame `0x0042DC40` の this）。`0x0042E6A1 mov $0x4b30b0,%ecx` の直後に `0x0042E6A6 call 0x42dc40`。
/// 入口は静的変数（`stub::WINDOW`）から読むので、検査では偽のウィンドウを指せる。
pub const WINDOW_ADDR: u32 = 0x004B_30B0;
/// ウィンドウの今の時刻（double）。do_frame の先頭で時計 `0x0042CC90` の値を `0x0042DC4C fstl 0x2c(%esi)` で置く。同じ do_frame の中では変わらない。
pub const WINDOW_NOW: u32 = 0x2C;
/// ウィンドウの期限（double）。`0x0042DC59 fcoml 0x3c(%esi)` で今の時刻と比べ、過ぎていれば `0x0042DC74`〜`0x0042DC7D` で 1 周期ずつ進める。
pub const WINDOW_DEADLINE: u32 = 0x3C;
/// ウィンドウの時計の分母（QueryPerformanceFrequency の下位 32 ビット。0 なら timeGetTime の経路）。時計 `0x0042CC96 mov 0x4b30c4,%eax`、
/// 書く側 `0x0042E4E2 push $0x4b30c4`。読み取りのスレッドが 1 回だけ読んでログに書く。
pub const WINDOW_CLOCK_DIVISOR: u32 = 0x14;
/// 入口がウィンドウから読む範囲の長さ（`+0x3C` の double の終わりまで）。設置の前にここまで読めるかを見る。
pub const WINDOW_SPAN: u32 = WINDOW_DEADLINE + 8;
const _: () = assert!(WINDOW_NOW + 8 <= WINDOW_SPAN);
/// on_tick `0x00431110` の this の、tick ごとに 1 増える欄。on_tick の終わり `0x00431340 mov 0x454(%esi),%ecx` → `0x0043134C inc %ecx` →
/// `0x00431355 mov %ecx,0x454(%esi)`。入口の時点の esi は on_tick の this。
pub const SUP_FRAME: u32 = 0x454;
/// on_tick の this の今の場面。`0x004311F5 mov 0x58c(%esi),%ecx` → `0x00431218 jmp *0x43145c(,%ecx,4)` で場面ごとに分かれ、
/// 終わりの `0x0043134F mov %edx,0x58c(%esi)` で次の場面の要求をコピーする。
pub const SUP_SCENE_CUR: u32 = 0x58C;
/// on_tick の this の次の場面の要求。`0x0043133A mov %edi,0x590(%esi)` と `0x00431346 mov 0x590(%esi),%edx`。
pub const SUP_SCENE_REQ: u32 = 0x590;

// ---- 観測のバージョン（選択とマッチ）で入口がコピーする範囲と、読み取りのスレッドがたどるアドレス ----
// 検査 `observation_addresses_appear_in_the_design` が設計の本文と突き合わせる。コピーする範囲はどれも th09.exe の .data
// （VA 0x004A0000〜0x004E41FC。VirtualSize 0x441FC）の中で、起動から終わりまでコミット済み・読み書きできる頁（Characteristics 0xC0000040）。
// 入口は読むだけで書かない。

/// 席のブロック 2 つ（0x38 刻み）から、選択・読み込み・マップ・一時停止までのグローバルの範囲の始まり。
pub const SEATS_RANGE: u32 = 0x004A_7D90;
/// その長さ（`0x4A7D90`〜`0x4A7ED0` の 320 バイト）。
pub const SEATS_LEN: u32 = 320;
/// 初期ライフ・読み込みの終わり・対戦の登録を含む範囲の始まり（`0x4A8100`〜`0x4A8130`）。
pub const MATCH_RANGE: u32 = 0x004A_8100;
pub const MATCH_LEN: u32 = 48;
/// 乱数（`0x42AE50` の this）の 12 バイト。`+0x00` が状態（u16）、`+0x04` が抽選回数（`0x004ACE10`）。
pub const RNG_ADDR: u32 = 0x004A_CE0C;
pub const RNG_LEN: u32 = 12;
pub const RNG_COUNT: u32 = 0x004A_CE10;
/// InputManager 3 本（1P / 2P / 合成）。`0x004ACE18` から 0x8E 刻みなので、2 本目 `0x004ACEA6` は 4 バイト境界に無い
/// （入口の `rep movsb` はバイト単位なので構わない）。
pub const INPUT_MANAGERS: [u32; 3] = [0x004A_CE18, 0x004A_CEA6, 0x004A_CF34];
/// 1 本からコピーする長さ（`+0x00`〜`+0x37`）。
pub const INPUT_COPY_LEN: u32 = 0x38;
// InputManager の欄（どれも u16）。
pub const IM_CUR: u32 = 0x00;
pub const IM_PREV: u32 = 0x02;
pub const IM_REPEAT: u32 = 0x04;
pub const IM_DOWN: u32 = 0x06;
pub const IM_UP: u32 = 0x08;
pub const IM_CHARGE: u32 = 0x2A;
pub const IM_GAME: u32 = 0x2C;
/// ゲーム自身のスクリーンショットが見る語（合成 `[2]` の押した瞬間 `+0x06` = `0x004ACF3A`）と、P・Home のビット。
/// 読むのは Present のラッパ `0x0042DB50` の中の `0x0042DBAA`（`[2]+0x06 & 0x0800`。ノート `th09_title_and_d3d8.md` §2-6・`th09_input_and_wait_A0.md` §2。
/// 逆アセンブルの読みで実機とは未照合）。`shot_key_win` が、自分の PC で P/Home を押した tick の Present のラッパを呼ぶ間だけ立てて戻す。
pub const SHOT_WORD: u32 = INPUT_MANAGERS[2] + IM_DOWN;
pub const SHOT_BIT: u16 = 0x0800;

// 範囲の中のグローバル（CSV の列にするもの。値は u32 として読む）。
/// 1P / 2P の Player へのポインタ（読み取りのスレッドがライフをたどる起点）。
pub const P1_PLAYER_PTR: u32 = 0x004A_7D94;
pub const P2_PLAYER_PTR: u32 = 0x004A_7DCC;
pub const P1_CHAR: u32 = 0x004A_7DB0;
pub const P2_CHAR: u32 = 0x004A_7DE8;
/// 人 / CPU（Match の中で 0 か）。
pub const P1_CONTROL: u32 = 0x004A_7DB8;
pub const P2_CONTROL: u32 = 0x004A_7DF0;
pub const BACKGROUND: u32 = 0x004A_7E84;
/// ゲームのウィンドウが前面（アクティブ）か（WM_ACTIVATEAPP の wParam をウィンドウの手続き `0x0042D425` が `mov %ebx,0x4b30b8` で置く）。0 ならゲーム自身が
/// キーボードを読まない（入力の関数 `0x0042B850` が先頭の `0x0042B859` で読み、`0x0042B873 test` の後の `je` で飛ぶ。`0x0042BE40` も同じ）。
/// 数字キーの読み（`digit_keys`）が、ウィンドウが前面にあるときだけ読むために同じ旗を見る。
pub const APP_ACTIVE: u32 = 0x004B_30B8;
/// Match のキャラ選択の分かれ目（`[0x4A7EA4]==0` / 1〜3）。
pub const SELECT_KIND: u32 = 0x004A_7EA4;
pub const GAME_MODE: u32 = 0x004A_7EA8;
pub const DIFFICULTY: u32 = 0x004A_7EAC;
pub const LOADING: u32 = 0x004A_7EB4;
pub const MAP: u32 = 0x004A_7EC8;
/// グローバル状態ビット語（bit1 = タイトルデモ再生中 / bit3 = リプレイ再生中）。描画だけのフレームがデモの間かどうかを分けるのに読む
/// （デモの間は `& 0x0E`、bit1 が立つ）。`SEATS_RANGE` の中なので入口がコピーする。
pub const GLOBAL_STATE: u32 = 0x004A_7EC4;
pub const PAUSE: u32 = 0x004A_7ECC;
pub const LOAD_DONE: u32 = 0x004A_811C;
pub const MATCH_REGISTERED: u32 = 0x004A_8120;
/// Player の `life_raw`（0〜10、u32）。
pub const PLAYER_LIFE: u32 = 0xA8;

// ---- ラウンドごとの記録（リプレイの印。境目の tick で読む）----
/// ラウンドの終わりの関数 `0x00415D70` が最初に増やす数（`0x00415D81 incl 0x4a7e90`）。続けて同じ関数の中で勝者の勝ち数（`P1_WINS` / `P2_WINS`）も
/// 増やす（`0x00415D8A`〜`0x00415D92`）ので、増えた tick に勝者が決まっている。マッチの始まりに読み込みスレッドが 0 に戻す（`0x0041AF4F` 付近）。
pub const COMPLETED_ROUNDS: u32 = 0x004A_7E90;
/// ラウンドの経過フレーム（隣リポ Memory_Map の `round_frames_a`、`th09_monitor.py` の `ADDR["round_frames_a"]`）。
pub const ROUND_FRAMES: u32 = 0x004A_7E40;
/// Player のカウンタ（u32。Player の生成 `0x0041EFC7`〜`0x0041EFD3` が 0 にして、以後はマッチ通しに増える）。
/// C2 と C3 の数: `0x00404023 incl 0xb0(%eax)`。
pub const PLAYER_SPELL_ATTACKS: u32 = 0xB0;
/// ボスアタックの数: `0x00403EDF incl 0xb4(%eax)`。
pub const PLAYER_BOSS_ATTACKS: u32 = 0xB4;
/// リバーサルの数（ボスアタックの内数）: `0x00403ED3 incl 0xb8(%eax)`。
pub const PLAYER_BOSS_REVERSALS: u32 = 0xB8;
/// 1P / 2P のスコアなどを持つ塊（0xA0 バイト）へのポインタ。読み込みスレッドが `0x0041B069 mov $0x4a7dac,%esi` から 0x38 刻みで置く。
pub const P1_STATS_PTR: u32 = 0x004A_7DAC;
pub const P2_STATS_PTR: u32 = 0x004A_7DE4;
/// 塊の中のスコア（u32。画面に出る値の 1/10 —— 隣リポ `th09_monitor.py` の `STATS["score"]`、`s1a*10`）。
pub const STATS_SCORE: u32 = 0x04;

// ---- 台本の状態の操作（無敵・ライフ・ゲージ。検査の道具）----
// 書くのは台本が動いている実行だけで、`P1_PLAYER_PTR` / `P2_PLAYER_PTR` が 0 でなく `LOADING` が 0 の tick に、入力の地点で書く（`state_ops.rs`）。
// Player へのポインタの欄はマッチの片付け（`0x0041B82A`）の中で、Player の解放 `0x0041B934 call 0x41ced0` の直後の `0x0041B93B mov %edi,-0x4(%esi)` で 0 に戻るので、
// 0 でなければ生きた Player を指す（片付けがどのスレッドで走るかは未確認で、読み書きが競合しうる）。書き込みは自分のプロセスへの
// `WriteProcessMemory` で、アドレスが無ければ失敗で返るだけで落ちない。検査 `state_control_offsets_appear_in_the_note`。
/// Player の無敵の zTimer の今の値（float）。毎 tick の減算 `0x0042F490` が `current_f` から int を作り直す。
pub const PLAYER_INV_TIMER_F: u32 = 0x1B78;
/// 同じ zTimer の今の値（int）。被弾の判定の入口 `0x0041E8BC` が「0 以下なら判定へ」で読む。
pub const PLAYER_INV_TIMER_I: u32 = 0x1B7C;
/// 無敵のために毎 tick 置く値（float は 2.0、int は 2）。減算の後も 1 > 0 で判定を通らず、やめれば次の tick の減算で 0 に戻る。
pub const INV_TIMER_VALUE: u32 = 2;
/// Player のカードアタックのゲージ（float。0〜400）。溜める関数 `0x0041BC90` の `flds` / `fadds` / `fsts 0x30388(%esi)` と頭打ち
/// `0x0041BCF6 movl $0x43c80000,0x30388(%esi)`（= 400.0f）、クイックの処理 `0x0041CAB4`〜`0x0041CABF` の `mov $0x42c80000`（= 100.0f）から
/// 4 バイトの float。
pub const PLAYER_GAUGE: u32 = 0x30388;
/// ゲージの上限（上の頭打ちの 400.0f）。
pub const GAUGE_MAX: u32 = 400;
// ライフ `PLAYER_LIFE` の幅は 4 バイトの int: 被弾の処理 `0x0041E46A mov 0xa8(%esi),%eax`・`0x0041E5FE mov %eax,0xa8(%esi)`・
// `0x0041E632 movl $0x1,0xa8(%esi)`、ラウンドの初期化 `0x0041ED4A mov %ecx,0xa8(%esi)`（初期ライフ `0x4A8108 + 席 × 4` から）。
// 台本で書く範囲は初期ライフの範囲 `LIFE_MIN`〜`LIFE_MAX`。

/// MainMenu へのポインタ。解放された後も 0 に戻らないので、入口ではたどらない。読み取りのスレッドが落ちない読み方（`ReadProcessMemory`）でだけたどる。
pub const MAIN_MENU_PTR: u32 = 0x004A_C8C0;
// MainMenu の欄（`[0x004AC8C0]` からのオフセット。すべて u32）。
pub const MENU_CURSOR: u32 = 0x00;
pub const MENU_SUBSTATE: u32 = 0x28;
pub const MENU_PREV_SCREEN: u32 = 0x84;
pub const MENU_SCREEN_ID: u32 = 0x1B228;
/// 毎フレーム進む（変化の判定には使わず値だけ残す）。
pub const MENU_ANIM: u32 = 0x1B230;
pub const MENU_TRANSITIONING: u32 = 0x1B238;

// 読み取りのスレッドだけがたどる 3 欄（入口には足さない。どれも u32。ポインタは .data の固定のアドレスで、先はヒープ）。
// 原本は `0x00420470`〜`0x004204AA`・`0x0041B0D4`・`0x004216D4`〜`0x00421748`。
/// ReplayManager へのポインタ。
pub const REPLAY_MANAGER_PTR: u32 = 0x004A_7E64;
/// ReplayManager のカウンタ。再生のときだけ登録される優先度 6 のノードが 1 パスごとに 1 増やし、優先度 27 の `0x00420470` が `% 3` で
/// 早送りを決める（`0x00420490 mov (%esi),%eax`）。パスごとに変わるので、変化の判定には使わず値だけ残す。
pub const REPLAY_COUNTER: u32 = 0x000;
/// Live = 0 / Replay = 1（`[0x004A7E64] + 0x114`）。
pub const REPLAY_MODE: u32 = 0x114;
/// Gui へのポインタ（`0x00420481 mov 0x4a7e38,%ecx`）。
pub const GUI_PTR: u32 = 0x004A_7E38;
/// Gui の会話の状態。`0x00404980` が `mov 0xe94c(%ecx),%eax` で読み、0 以上か −2 なら 1（会話中）を返す。
pub const GUI_TALK: u32 = 0xE94C;

// ---- D3D8 のデバイスの下見（読むだけ）----
// `[0x004B3108]` が IDirect3DDevice8、vtable の `0x3C Present` / `0x8C EndScene`。読み取りのスレッドがアドレスとモジュールのパスをログに書くだけで、
// デバイスの関数は呼ばない（ゲームのスレッドの外から呼ぶのは安全でない）。
/// IDirect3DDevice8 へのポインタ（描画の入口が読む `mov eax,[0x4B3108]` と同じアドレス）。
pub const D3D8_DEVICE_PTR: u32 = 0x004B_3108;
/// vtable の中の EndScene / Present のスロット（バイト）。
pub const D3D8_ENDSCENE_SLOT: u32 = 0x8C;
pub const D3D8_PRESENT_SLOT: u32 = 0x3C;

// ---- 撮影（描画の入口の中で、ゲームのスレッドがバックバッファをコピーする）----
// ゲーム自身の撮影 `0x0042F540` の読み方に合わせる。検査 `shot_addresses_appear_in_the_note` がノートの本文と突き合わせる。
/// デバイスの GetBackBuffer（引数 0, 0, &面）。`0x0042F55E` `call *0x40(%ecx)`。
pub const D3D8_GETBACKBUFFER_SLOT: u32 = 0x40;
/// 面の LockRect（&locked, 矩形 NULL, フラグ 0）。`0x0042F65C` `call *0x24(%ecx)`。
pub const D3D8_SURF_LOCKRECT_SLOT: u32 = 0x24;
/// 面の UnlockRect。`0x0042F6C1` `call *0x28(%ecx)`。
pub const D3D8_SURF_UNLOCKRECT_SLOT: u32 = 0x28;
/// 面の GetDesc。IDirect3DSurface8 の並び（IUnknown の 3 つ・GetDevice・Set/Get/FreePrivateData・GetContainer・GetDesc・LockRect・UnlockRect）で
/// LockRect の 1 つ前（SDK の並びからの推定。ゲームは呼ばない）。
pub const D3D8_SURF_GETDESC_SLOT: u32 = 0x20;
/// 面の Release（IUnknown の 3 番目。SDK の並びからの推定）。
pub const D3D8_SURF_RELEASE_SLOT: u32 = 0x08;
/// present parameters のコピー（デバイスを作った後、`0x0042E089` の `rep movsl` で 13 語をコピーする先 `0x4B3450`。Reset もここを渡す）。
/// 幅 `+0` / 高さ `+4` / 形式 `+8`（D3DPRESENT_PARAMETERS の並び。ゲームの撮影は `[Supervisor+0x358]` = `0x4B3458` で形式を読む）。
pub const D3D8_PRESENT_PARAMS: u32 = 0x004B_3450;
pub const PP_WIDTH: u32 = 0x0;
pub const PP_HEIGHT: u32 = 0x4;
pub const PP_FORMAT: u32 = 0x8;

// ---- 既定の形（`mode = auto`）が先頭で読む画面の形（読むだけ）----
/// 設定のブロック `0x004B3488` の `+0xB1`（u8。0 = 全画面・1 = ウィンドウモード）。デバイスを作る関数の `0x0042DDC2 cmp %bl,0x4b3539` → 0 でなければ `0x0042DEA1` で Windowed = 1。
/// 値の読み分けは `higanr_core::pacing::screen_from_flag`。present parameters のコピーの Windowed（`D3D8_PRESENT_PARAMS` の `+0x1C`）の方が
/// 「いまのデバイス」に近いが、そのアドレスは資料に無く足し算で作ることになるので使わない。
pub const OPTION_WINDOWED: u32 = 0x004B_3539;

// ---- 台本（入力の入口の中で、ゲームのスレッドが画面の組を読み、feed を呼ぶ）----
/// Supervisor（on_tick `0x00431110` の this）。今の場面は `[SUPERVISOR + SUP_SCENE_CUR]`。実機で入口の esi はいつもこの値だった。
pub const SUPERVISOR: u32 = 0x004B_3100;
/// グローバル状態ビット語 `GLOBAL_STATE` の bit1（タイトルデモ再生中）。台本の `demo=` と集計のデモの判定がここを引く（値は 1 か所）。
pub const GLOBAL_STATE_DEMO: u32 = 0x2;
/// InputManager のリピートのカウンタ `cnt[b]`（u16 × 16）の始まり（`+0x0A + 2*b`）。
pub const IM_REPEAT_COUNTERS: u32 = 0x0A;
/// feed `0x0042ACE0` が書く範囲の長さ（`+0x00`〜`+0x29`。`+0x2A` 以降には書かない）。`+0x2A` は次の欄（`IM_CHARGE`）の始まりなので、その値を使う。
pub const IM_FEED_LEN: u32 = IM_CHARGE;
// 16 本のカウンタの終わりが、feed が書く範囲の終わりと一致する（建てるときに確かめる）。
const _: () = assert!(IM_REPEAT_COUNTERS + 16 * 2 == IM_FEED_LEN);
/// 台本が呼ぶゲームの関数の行（地点の表の「照合だけ」の行。アドレスはこの名前の行から引き、ここには転記しない）。
/// feed は fastcall（ecx = 番号、edx = 語）、ローカル入力は引数なし・戻り値 ax。
pub const FEED_SITE: &str = "fn_feed";
pub const LOCAL_INPUT_SITE: &str = "fn_local_input";
/// 素の poll `0x0042B850`（`POLL_SITES` の行の名前）。ネットワークの対戦で自分の入力を「1P の設定どおり」に読むために、席 0 で呼ぶ（ecx = 0）。
pub const POLL_SITE: &str = "fn_poll";

/// 素の poll の入口の照合の表（1 行）。`SITES` に入れない: `SITES` は全行の一致が設置の条件の表で、足すとネットワークを使わない実行までフックが止まる。
/// `session::run` がネットワークのセッションを置けた実行で照合し、合ったときだけ入口へ渡す。合わなければローカル入力は `fn_local_input`（`0x0042BE40`）で読む。
pub const POLL_SITES: &[SiteDef] = &[SiteDef {
    name: POLL_SITE,
    addr: 0x0042_B850,
    expected: &[0x55, 0x8B, 0xEC, 0x81, 0xEC, 0x04, 0x01, 0x00],
    kind: Use::VerifyOnly { instr_lens: None },
    source: site_source!("ノート th09_input_and_wait_A0.md §8（th09_disasm.txt L55096〜55098）"),
}];

// ---- ネットワーク（止める地点で揃った値を書く・開始のフックでチャージ型を書く・CHECK の値を読む）----
// アドレスは検査 `netplay_addresses_appear_in_the_design`、範囲は `select_ranges_appear_in_the_note` が突き合わせる。
/// 色（1P / 2P）。
pub const P1_COLOR: u32 = 0x004A_7DC0;
pub const P2_COLOR: u32 = 0x004A_7DF8;
/// 初期ライフ（1P / 2P）。
pub const P1_LIFE_INIT: u32 = 0x004A_8108;
pub const P2_LIFE_INIT: u32 = 0x004A_810C;
/// オプションブロック（0xCC バイト。終了時に th09.cfg へ書き出されるので**書かない**）。自分のチャージ型と
/// 意味の分からない欄のダイジェストを読むだけ。
pub const OPTIONS_BLOCK: u32 = 0x004B_3488;
/// マッチが読むオプションのコピーへのポインタ（読み込みスレッドがマッチごとに `OPTIONS_BLOCK` から作り直す）。
pub const OPTIONS_COPY_PTR: u32 = 0x004A_7E78;
/// ブロックとコピーの中のチャージ型（`+0xB4` が 1P、`+0xB5` が 2P。自分の値は `+0xB4`）。
pub const OPT_CHARGE: u32 = 0xB4;
/// ブロックの中の入力機器（`+0xB7` が 1P、`+0xB8` が 2P。0 / 1 = パッド、2〜4 = キーボードの配置）。読むだけ。
pub const OPT_INPUT_DEVICE: u32 = 0xB7;
/// 1P の入力機器（`0x004B353F`）。
pub const INPUT_DEVICE_P1: u32 = OPTIONS_BLOCK + OPT_INPUT_DEVICE;
/// SELECT のダイジェストに入れる、意味の分からない欄（ブロックの中の `[始め, 終わり)`。`+0x6C`〜`+0xA3`・`+0xB3`・`+0xB6`・`+0xB9`）。
pub const OPTION_DIGEST_RANGES: [(u32, u32); 4] =
    [(0x6C, 0xA4), (0xB3, 0xB4), (0xB6, 0xB7), (0xB9, 0xBA)];
/// 開始のフックの行（call 先の差し替え。`0x0041AB2B`）とシードを置く関数の行（照合だけ。thiscall、ecx = 乱数 `RNG_ADDR`、引数 u16 を 1 つ。
/// `0x00420160`〜`0x0042016B` は `[ecx]` に u16 を書くだけで、抽選回数は変えない）。アドレスはこの名前の行から引く。
pub const MATCH_START_SITE: &str = "match_start_call";
pub const SET_STATE_SITE: &str = "fn_set_state";
/// SELECT の値の範囲（人対人の Match でゲーム自身が書く値）。
pub const CHARA_MAX: u8 = 15;
pub const COLOR_MAX: u8 = 1;
pub const LIFE_MIN: u8 = 1;
pub const LIFE_MAX: u8 = 10;
pub const CHARGE_MAX: u8 = 1;
pub const DIFFICULTY_MAX: u8 = 3;
/// マップは 0〜`MAP_MAX` と、一覧の最後の 2 項目の −1 / −2（i8 として読む）。
pub const MAP_MAX: i8 = 13;
pub const MAP_RANDOM_OF_TWO: i8 = -1;
pub const MAP_RANDOM_ANY: i8 = -2;

// ---- ネットの選択（選択の 3 画面で、カーソルと席の旗を読み、押下を入れる前にカーソルを合わせる）----
// 難易度とマップのカーソルは `MENU_CURSOR`、画面は `MENU_SCREEN_ID`、押下を読むかは `MENU_SUBSTATE`（1 のとき）と `MENU_TRANSITIONING`（0 のとき）。
/// MainMenu の中の、キャラ選択のカーソル（1P。2P は `+4`。添字 0〜15）。
pub const MENU_CHARA_CURSOR: u32 = 0x10;
/// MainMenu の中の、キャラ選択の席の旗（1P。2P は `+4`。0 = 選んでいる / 1 = 決めた）。
pub const MENU_CHARA_DECIDED: u32 = 0x18;
/// マップの一覧（表 `0x4A1D9C` のうち出す項目だけを前から詰め、残りは 0xFF。th09.exe の .bss）。
pub const MAP_LIST: u32 = 0x004A_C8C4;
pub const MAP_LIST_LEN: u32 = 16;

// ---- 立ち絵の追随（決定の tick にカーソルを書いたとき、立ち絵と席の目印を動かす 3 本の呼び出し）----
// キャラ選択のハンドラ `0x004289DB` の移動の経路（1P は `0x00428A5D`〜`0x00428AE3`、2P は `0x00428BDB`〜`0x00428C61`）が呼ぶのと同じ引数を、dll がカーソルを書いた直後に呼ぶ。
/// MainMenu の中の部品の配列へのポインタ（`0x00424F0C mov 0x11b98(%edi),%eax`。部品は 1 つ 0x2A4 バイト）。0 なら ANM が読まれておらず、立ち絵の関数は何もしない（`0x00436AC0`）ので呼ばない。
pub const MENU_PARTS_PTR: u32 = 0x11B98;
/// キャラの解放の印（バイト 16 本。`[CHARA_UNLOCK + キャラ]` が 0 でなければ手元で解放済み。`0x0042306E` / `0x00423098 cmpb $0x0,0x4a81cc(%ecx)`）。
pub const CHARA_UNLOCK: u32 = 0x004A_81CC;
/// キャラ選択のカーソル → キャラの表。`0x00425233` の第 4 引数（`0x00428A45 mov $0x4a1e2c,%edi`）。
pub const CHARA_TABLE: u32 = 0x004A_1E2C;
/// 立ち絵の部品の番号の基（席 s は `+ s`。`0x00428A9A push $0x5c` / `0x00428AAF push $0x3c`、2P は `0x5d` / `0x3d`）。
pub const PORTRAIT_BASE_A: u32 = 0x5C;
pub const PORTRAIT_BASE_B: u32 = 0x3C;
/// 立ち絵の関数の第 3・第 4 引数（部品の数 16 = `0x00428A42 push $0x10; pop %ebx`、間隔 2 = `0x00428A94 push $0x2`）。
pub const PORTRAIT_COUNT: u32 = 0x10;
pub const PORTRAIT_STRIDE: u32 = 2;
/// 立ち絵の 3 本の関数の行（`PORTRAIT_SITES`）の名前。thiscall（ecx = MainMenu `[0x4AC8C0]`）、引数は 4 つで `ret 0x10`
/// （`0x00424FAD` / `0x0042505C` / `0x0042538D`）。アドレスはこの名前の行から引く。`SLIDE` = `0x00424EDD`（割り込み 8 / 7。delta > 0 の 1P・delta < 0 の 2P）、
/// `SLIDE_BACK` = `0x00424FB0`（割り込み 0xF / 0xE。その逆の向き）、`MARKER` = `0x00425233`（席の目印とアイコンの色の補間）。
pub const PORTRAIT_SLIDE_SITE: &str = "fn_portrait_slide";
pub const PORTRAIT_SLIDE_BACK_SITE: &str = "fn_portrait_slide_back";
pub const PORTRAIT_MARKER_SITE: &str = "fn_portrait_marker";

/// 立ち絵の 3 本の関数の入口の照合の表。`SITES` に入れない（ここが合わないときに止めたいのは立ち絵の追随だけ）。`session::run` がネットワークを置いた実行で
/// 全行を照合し、全部合ったときだけアドレスを入口へ渡す。スライドの 2 本は先頭 32 バイトまで同じで 33 バイト目（`jge` の飛び幅。`0x62` と `0x4E`）で分かれる。
pub const PORTRAIT_SITES: &[SiteDef] = &[
    SiteDef {
        name: PORTRAIT_SLIDE_SITE,
        addr: 0x0042_4EDD,
        expected: &[
            0x55, 0x8B, 0xEC, 0x51, 0x8B, 0x45, 0x0C, 0x53, 0x8B, 0x5D, 0x14, 0x56, 0x57, 0x8B,
            0xF9, 0x8B, 0x4D, 0x10, 0x0F, 0xAF, 0xCB, 0x03, 0xC8, 0x3B, 0xC1, 0x89, 0x45, 0xFC,
            0x89, 0x4D, 0x10, 0x7D, 0x62,
        ],
        kind: Use::VerifyOnly { instr_lens: None },
        source: site_source!(
            "ノート th09_select_portrait_M2-3.md §1（th09_disasm.txt L47120〜47135）"
        ),
    },
    SiteDef {
        name: PORTRAIT_SLIDE_BACK_SITE,
        addr: 0x0042_4FB0,
        expected: &[
            0x55, 0x8B, 0xEC, 0x51, 0x8B, 0x45, 0x0C, 0x53, 0x8B, 0x5D, 0x14, 0x56, 0x57, 0x8B,
            0xF9, 0x8B, 0x4D, 0x10, 0x0F, 0xAF, 0xCB, 0x03, 0xC8, 0x3B, 0xC1, 0x89, 0x45, 0xFC,
            0x89, 0x4D, 0x10, 0x7D, 0x4E,
        ],
        kind: Use::VerifyOnly { instr_lens: None },
        source: site_source!(
            "ノート th09_select_portrait_M2-3.md §1（th09_disasm.txt L47187〜47202）"
        ),
    },
    SiteDef {
        name: PORTRAIT_MARKER_SITE,
        addr: 0x0042_5233,
        expected: &[
            0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x10, 0x53, 0x56, 0x57, 0x8B, 0xF9, 0x8B, 0x4D, 0x08,
            0x81, 0xC1, 0xBD, 0x00, 0x00, 0x00, 0x69, 0xC9, 0xA4, 0x02, 0x00, 0x00,
        ],
        kind: Use::VerifyOnly { instr_lens: None },
        source: site_source!(
            "ノート th09_select_portrait_M2-3.md §1（th09_disasm.txt L47372〜47382）"
        ),
    },
];

/// 地点の表（14 行）。
pub const SITES: &[SiteDef] = &[
    // Supervisor::on_tick の中の call 0x0042E9C0（poll_all）。入力と待ちの差し替え先。
    SiteDef {
        name: "input_wait_call",
        addr: 0x0043_112D,
        expected: &[0xE8, 0x8E, 0xD8, 0xFF, 0xFF],
        kind: Use::CallRel32 {
            original_target: 0x0042_E9C0,
        },
        source: site_source!("ノート §1-1（th09_disasm.txt L62819〜L62828）"),
    },
    // EndScene の直前の mov 0x4b3108,%eax（描画のフック）。
    SiteDef {
        name: "draw_before_endscene",
        addr: 0x0042_DD51,
        expected: &[0xA1, 0x08, 0x31, 0x4B, 0x00],
        kind: Use::Detour { instr_lens: &[5] },
        source: site_source!("ノート §1-2（th09_disasm.txt L58512〜L58521）"),
    },
    // タイトルの「端で下」: xor %ebx,%ebx; push %ebx; push $0xc。
    SiteDef {
        name: "title_down_at_edge",
        addr: 0x0042_45D5,
        expected: &[0x33, 0xDB, 0x53, 0x6A, 0x0C],
        kind: Use::Detour {
            instr_lens: &[2, 1, 2],
        },
        source: site_source!("ノート §1-3（th09_disasm.txt L46341〜L46346）"),
    },
    // タイトルの「先頭で上」。「端で下」と上下対称で同じバイト列。
    SiteDef {
        name: "title_up_at_top",
        addr: 0x0042_45A2,
        expected: &[0x33, 0xDB, 0x53, 0x6A, 0x0C],
        kind: Use::Detour {
            instr_lens: &[2, 1, 2],
        },
        source: site_source!("ノート §1-4（th09_disasm.txt L46321〜L46326）"),
    },
    // 決定の横取り: testw $0x1001,0x4acf3a（接頭辞 66 込みで 1 命令 9 バイト）。
    SiteDef {
        name: "title_decide",
        addr: 0x0042_A1C8,
        expected: &[0x66, 0xF7, 0x05, 0x3A, 0xCF, 0x4A, 0x00, 0x01, 0x10],
        kind: Use::Detour { instr_lens: &[9] },
        source: site_source!("ノート §1-5（th09_disasm.txt L53113〜L53119）"),
    },
    // poll の中の cur 読み出し: xor %eax,%eax; mov (%edi),%ax。観測専用で書かない。
    SiteDef {
        name: "poll_cur_read",
        addr: 0x0042_BE0B,
        expected: &[0x33, 0xC0, 0x66, 0x8B, 0x07],
        kind: Use::VerifyOnly {
            instr_lens: Some(&[2, 3]),
        },
        source: site_source!("ノート §1-6（th09_disasm.txt L55535〜L55541）"),
    },
    // リプレイ保存の検知: push %ebx（1B）＋ call *0x48e05c（6B、絶対アドレスの間接 call）。5 バイトでは境目に届かないので 7 バイト。
    SiteDef {
        name: "replay_saved",
        addr: 0x0042_1124,
        expected: &[0x53, 0xFF, 0x15, 0x5C, 0xE0, 0x48, 0x00],
        kind: Use::Detour {
            instr_lens: &[1, 6],
        },
        source: site_source!("ノート §1-7（th09_disasm.txt L41934〜L41944）"),
    },
    // マッチの開始の call 0x43bf30。オペコードは 0x0041AB2B、rel32 は 0x0041AB2C。
    SiteDef {
        name: "match_start_call",
        addr: 0x0041_AB2B,
        expected: &[0xE8, 0x00, 0x14, 0x02, 0x00],
        kind: Use::CallRel32 {
            original_target: 0x0043_BF30,
        },
        source: site_source!("ノート §1-8（th09_disasm.txt L33589〜L33596）"),
    },
    // 読み込みスレッドを作る直前の call 0x431ae0（予備）。オペコードは 0x0041B811、rel32 は 0x0041B812。
    SiteDef {
        name: "loader_thread_call",
        addr: 0x0041_B811,
        expected: &[0xE8, 0xCA, 0x62, 0x01, 0x00],
        kind: Use::CallRel32 {
            original_target: 0x0043_1AE0,
        },
        source: site_source!("ノート §1-9（th09_disasm.txt L34473〜L34480）"),
    },
    // 描画だけのフレームの出口（pop %esi; mov $0x3,%eax; pop %ebx; ret）。戻りアドレスとして照合だけ。
    SiteDef {
        name: "render_only_exit",
        addr: 0x0043_1259,
        expected: &[0x5E, 0xB8, 0x03, 0x00, 0x00, 0x00, 0x5B, 0xC3],
        kind: Use::VerifyOnly {
            instr_lens: Some(&[1, 5, 1, 1]),
        },
        source: site_source!("ノート §1-10（th09_disasm.txt L62894〜L62900）"),
    },
    // 呼ぶ前に照合する関数の入口の先頭 8 バイト（4 行）。末尾は命令の途中で切れる。
    SiteDef {
        name: "fn_feed",
        addr: 0x0042_ACE0,
        expected: &[0x55, 0x8B, 0xEC, 0x51, 0x69, 0xC9, 0x8E, 0x00],
        kind: Use::VerifyOnly { instr_lens: None },
        source: site_source!("ノート §1-11（th09_disasm.txt L53995〜54000）"),
    },
    SiteDef {
        name: "fn_local_input",
        addr: 0x0042_BE40,
        expected: &[0x55, 0x8B, 0xEC, 0x81, 0xEC, 0x00, 0x01, 0x00],
        kind: Use::VerifyOnly { instr_lens: None },
        source: site_source!("ノート §1-11（th09_disasm.txt L55570〜55573）"),
    },
    SiteDef {
        name: "fn_poll_all",
        addr: 0x0042_E9C0,
        expected: &[0x33, 0xC9, 0xE8, 0x89, 0xCE, 0xFF, 0xFF, 0xB9],
        kind: Use::VerifyOnly { instr_lens: None },
        source: site_source!("ノート §1-11（th09_disasm.txt L59478〜59480）"),
    },
    SiteDef {
        name: "fn_set_state",
        addr: 0x0042_0160,
        expected: &[0x55, 0x8B, 0xEC, 0x66, 0x8B, 0x45, 0x08, 0x66],
        kind: Use::VerifyOnly { instr_lens: None },
        source: site_source!("ノート §1-11（th09_disasm.txt L40635〜40638）"),
    },
];

/// 禁止の範囲（半開区間 `[start, end)`）: vpatch のパッチ 5 箇所・飛ばす範囲 2 つ・tick のジャンプ表の手前。vpatch の書き込み範囲は、書き始める位置から書くバイト数ぶん
/// （`0x0042DC45` と `0x0047D5DF` は call の rel32 の位置で、オペコード `E8` は範囲に入れていない）。残る 3 か所の飛ばす範囲は未確定（vpatch.dll の解析が要る）。
/// vpatch の `0x0042DC45`（7 バイト）とその飛ばす範囲 `0x0042DC4C`〜`0x0042DC8C` の 2 行は、ペース B の頭の入口が使う（`PACING_SITES`）ので外した。
pub const FORBIDDEN: &[AddrRange] = &[
    // vpatch 0x0042DB59: 7 バイト。
    AddrRange {
        start: 0x0042_DB59,
        end: 0x0042_DB60,
    },
    // その飛ばす範囲 0x0042DB60〜0x0042DB99（両端を含む。着地は 0x0042DB9A）。
    AddrRange {
        start: 0x0042_DB60,
        end: 0x0042_DB9A,
    },
    // vpatch 0x0042F7F7: 5 バイト。
    AddrRange {
        start: 0x0042_F7F7,
        end: 0x0042_F7FC,
    },
    // vpatch 0x004316C2: 6 バイト（末尾 1 バイトの中身は推定だが長さは原バイトから確定）。
    AddrRange {
        start: 0x0043_16C2,
        end: 0x0043_16C8,
    },
    // vpatch 0x0047D5DF: 4 バイト。
    AddrRange {
        start: 0x0047_D5DF,
        end: 0x0047_D5E3,
    },
    // tick の ret（0x0042C7B5）・整列の 8B FF・戻り値のジャンプ表（0x0042C7B8〜0x0042C7D3 の 28 バイト）。ret へ 5 バイト JMP を書くと表の 0 番目を潰す。
    AddrRange {
        start: 0x0042_C7B5,
        end: 0x0042_C7D4,
    },
];

// ---- 確保・解放を呼び出し元ごとに数える観測のバージョン ----
// 上の `SITES` とは別の表にする: `SITES` は起動のたびに全行を照合して全部一致したときだけ書く表で、行の数を縛る検査もある。
// ini で有効にした実行だけで書く実験なので、設置の判定も別に持つ（`alloc_count::install_all`）。ノートとの突き合わせは
// `alloc_count` の tests（`alloc_sites_appear_in_the_note`）。

/// E1-1 で inline detour を書く確保・解放の入口 5 つ。どれも関数の先頭（呼ばれた直後で `[esp]` が戻りアドレス）なので、
/// 入口は `[esp]` を呼び出し元として数える。並びは入口のアセンブリの地点の番号（0〜4）と同じ。
pub const ALLOC_SITES: &[SiteDef] = &[
    // 確保の包み（ecx = 0x4AD018、引数は大きさとタグ、`ret 8`）。中で 0x47B151 を呼ぶので、その分は crt_malloc でも
    // 呼び出し元 0x0040134C として数える（二重に数えた分は呼び出し元の列で分かる）。
    SiteDef {
        name: "alloc_wrap",
        addr: 0x0040_1340,
        expected: &[0x55, 0x8B, 0xEC, 0x8B, 0x45, 0x08],
        kind: Use::Detour {
            instr_lens: &[1, 2, 3],
        },
        source: site_source!("ノート §9（th09_disasm.txt L390〜392）"),
    },
    // 解放の包み（`ret 4`）。中で 0x47B163 を呼ぶ（crt_free の呼び出し元 0x0040136C）。
    SiteDef {
        name: "free_wrap",
        addr: 0x0040_1360,
        expected: &[0x55, 0x8B, 0xEC, 0x8B, 0x45, 0x08],
        kind: Use::Detour {
            instr_lens: &[1, 2, 3],
        },
        source: site_source!("ノート §9（th09_disasm.txt L411〜413）"),
    },
    // CRT の malloc（`push [0x4E2ADC]`（new の扱いの旗）; `push [esp+8]`; `call 0x47B125`）。盗むのは 1 命令目だけ。
    SiteDef {
        name: "crt_malloc",
        addr: 0x0047_B151,
        expected: &[0xFF, 0x35, 0xDC, 0x2A, 0x4E, 0x00],
        kind: Use::Detour { instr_lens: &[6] },
        source: site_source!("ノート §9（th09_disasm.txt L164770）"),
    },
    // operator new と見られる（0x47B125 を旗 1 で呼ぶ。`push 1`; `push [esp+8]`）。`[esp+8]` は esp 相対だが、入口は esp を入った時の値に
    // 戻してからトランポリンへ飛ぶので、盗んだ命令は同じアドレスを読む。
    SiteDef {
        name: "op_new",
        addr: 0x0047_B24E,
        expected: &[0x6A, 0x01, 0xFF, 0x74, 0x24, 0x08],
        kind: Use::Detour {
            instr_lens: &[2, 4],
        },
        source: site_source!("ノート §9（th09_disasm.txt L164853〜164854）"),
    },
    // CRT の free（SEH の前置き `push 0xC`; `push 0x4953F0` の後に `call 0x47F0B0`）。`0x47B249`（`jmp 0x47B163` の
    // 1 命令）から来た呼び出しも、`[esp]` は元の呼び出し元のまま。
    SiteDef {
        name: "crt_free",
        addr: 0x0047_B163,
        expected: &[0x6A, 0x0C, 0x68, 0xF0, 0x53, 0x49, 0x00],
        kind: Use::Detour {
            instr_lens: &[2, 5],
        },
        source: site_source!("ノート §9（th09_disasm.txt L164776〜164777）"),
    },
];

/// 勝ち数（1P / 2P。u32）。秒ごとの行にコピーするだけ（マッチがいくつ進んだかを後で数えるため）。
pub const P1_WINS: u32 = 0x004A_7E98;
pub const P2_WINS: u32 = 0x004A_7E9C;
/// マッチの終わりのメニューが出ているか（1 バイト。1 = 出ている）。書くのは `movb` の 4 か所（`0x4170A8` / `0x4178BE` が 1、`0x43560C` / `0x435A4A` が 0）。
pub const MATCH_END_MENU: u32 = 0x004A_7ECE;

// ---- 対戦が終わった後の戻り道（`higanr_core::return_flow`）が読むだけのメニューの状態 ----
// AsciiManager（`0x4CE458`）の on_tick `0x00435B00` が、一時停止のバイト `PAUSE` が 0 でなければ `0x00435BA8 lea 0x82a0(%esi),%ecx` →
// `call 0x434740`、`MATCH_END_MENU` が 0 でなければ `0x00435BD4 lea 0xa7a8(%esi),%ecx` → `call 0x4355F0`。どちらも状態が `+0x00`、
// フレームの数えが `+0x04`（関数の先頭の `mov %ecx,%esi` の後の `(%esi)` / `0x4(%esi)`）。実機で値を確かめていない。
/// 一時停止のメニュー（`0x4D66F8`）。
pub const PAUSE_MENU: u32 = 0x004C_E458 + 0x82A0;
/// マッチの終わりのメニュー（`0x4D8C00`）。
pub const END_MENU: u32 = 0x004C_E458 + 0xA7A8;
pub const MENU_OBJ_STATE: u32 = 0x00;
pub const MENU_OBJ_FRAMES: u32 = 0x04;
// 足し算の書き誤りを建てるときに落とす。ascii.anm のスロット（`+0x8288`）より後ろの、重ならない 2 つの欄。
const _: () = assert!(
    PAUSE_MENU == 0x004D_66F8
        && END_MENU == 0x004D_8C00
        && PAUSE_MENU > ASCII_ANM_LOADED_PTR
        && END_MENU > PAUSE_MENU + MENU_OBJ_FRAMES
);

// ---- 巻き戻しの可否実験: コピーする範囲・確保を DLL の領域へ向ける条件の値 ----
// 入口のアドレスと期待バイト列は上の `ALLOC_SITES` の `crt_malloc` / `op_new` / `crt_free` の行をそのまま使う（転記しない）。
// 転記ミスは `rollback_probe` の検査 `rollback_values_appear_in_the_note` が見る。

/// 決まった塊（読み込みスレッドが陣ごとに 1 回 operator new で取り、ポインタを zGameManager の陣の欄へ置く）。陣の欄は
/// `0x0041B209 mov $0x4a7dc4,%esi` から `add $0x38` で回る（`esi-0x34`〜`esi-0x1C`）。大きさは各 new の `push $imm32`。
pub const ROLLBACK_BLOCKS: &[RbBlock] = &[
    RbBlock {
        name: "p1_stage",
        ptr_at: 0x004A_7D90,
        size: 0x65C4,
        source: site_source!("0x0041B22B -0x34(%esi) / 0x00403AFC push"),
    },
    RbBlock {
        name: "p1_player",
        ptr_at: 0x004A_7D94,
        size: 0x30F70,
        source: site_source!("0x0041B242 -0x30(%esi) / 0x0041F18D push"),
    },
    RbBlock {
        name: "p1_bullets",
        ptr_at: 0x004A_7D98,
        size: 0x25E1C0,
        source: site_source!("0x0041B254 -0x2c(%esi) / 0x004150EB push"),
    },
    RbBlock {
        name: "p1_effects",
        ptr_at: 0x004A_7D9C,
        size: 0x2DC,
        source: site_source!("0x0041B26D -0x28(%esi) / 0x0040D326 push"),
    },
    RbBlock {
        name: "p1_enemies",
        ptr_at: 0x004A_7DA0,
        size: 0x2AC450,
        source: site_source!("0x0041B27F -0x24(%esi) / 0x0041225C push"),
    },
    RbBlock {
        name: "p1_spellcard",
        ptr_at: 0x004A_7DA4,
        size: 0x1368,
        source: site_source!("0x0041B291 -0x20(%esi) / 0x0040461B push"),
    },
    RbBlock {
        name: "p1_gui_side",
        ptr_at: 0x004A_7DA8,
        size: 0xABE8,
        source: site_source!("0x0041B2A3 -0x1c(%esi) / 0x0041A5EB push"),
    },
    // stats（スコア・残機など。GuiSide の数字の絵はここのスコアから作る）。読み込みスレッドの陣のループではなく、マッチの初期化 `0x0041AF2D` の「新規開始」の枝で
    // 確保する（マッチの前なので CRT のヒープにあり、DLL の領域の外）。`0x0041B069 mov $0x4a7dac,%esi` → `0x0041B07D push $0xa0` → `0x0041B082 call 0x47B24E`
    // → `0x0041B08A mov %edi,(%esi)` → `0x0041B09A rep stos` → `0x0041B090 add $0x38,%esi` / `cmp $0x4a7e1c` で 2 陣。
    RbBlock {
        name: "p1_stats",
        ptr_at: 0x004A_7DAC,
        size: 0xA0,
        source: site_source!(
            "0x0041B069 mov $0x4a7dac,%esi / 0x0041B07D push $0xa0 / 0x0041B08A mov %edi,(%esi)"
        ),
    },
    RbBlock {
        name: "p2_stage",
        ptr_at: 0x004A_7DC8,
        size: 0x65C4,
        source: site_source!("p1_stage の欄 + 0x38"),
    },
    RbBlock {
        name: "p2_player",
        ptr_at: 0x004A_7DCC,
        size: 0x30F70,
        source: site_source!("p1_player の欄 + 0x38"),
    },
    RbBlock {
        name: "p2_bullets",
        ptr_at: 0x004A_7DD0,
        size: 0x25E1C0,
        source: site_source!("p1_bullets の欄 + 0x38"),
    },
    RbBlock {
        name: "p2_effects",
        ptr_at: 0x004A_7DD4,
        size: 0x2DC,
        source: site_source!("p1_effects の欄 + 0x38"),
    },
    RbBlock {
        name: "p2_enemies",
        ptr_at: 0x004A_7DD8,
        size: 0x2AC450,
        source: site_source!("p1_enemies の欄 + 0x38"),
    },
    RbBlock {
        name: "p2_spellcard",
        ptr_at: 0x004A_7DDC,
        size: 0x1368,
        source: site_source!("p1_spellcard の欄 + 0x38"),
    },
    RbBlock {
        name: "p2_gui_side",
        ptr_at: 0x004A_7DE0,
        size: 0xABE8,
        source: site_source!("p1_gui_side の欄 + 0x38"),
    },
    RbBlock {
        name: "p2_stats",
        ptr_at: 0x004A_7DE4,
        size: 0xA0,
        source: site_source!("p1_stats の欄 + 0x38（0x0041B090 add $0x38,%esi のループの 2 周目）"),
    },
    RbBlock {
        name: "effects_3",
        ptr_at: 0x004A_7E0C,
        size: 0x2DC,
        source: site_source!("0x0041B2D1 mov %eax,0x4a7e0c（0x0041B2CA call 0x40D320）"),
    },
    RbBlock {
        name: "gui",
        ptr_at: 0x004A_7E38,
        size: 0x1293C,
        source: site_source!("0x0041B1F7 mov %eax,0x4a7e38 / 0x00418859 push"),
    },
    RbBlock {
        name: "ex_attack",
        ptr_at: 0x004A_7E3C,
        size: 0x4C80,
        source: site_source!("0x0041B2E3 mov %eax,0x4a7e3c / 0x004157E9 push"),
    },
    RbBlock {
        name: "replay_manager",
        ptr_at: REPLAY_MANAGER_PTR,
        size: 0x164,
        source: site_source!("0x0041B102 / 0x0041B11E mov %eax,0x4a7e64 / 0x00421677 push"),
    },
];

/// EffectSys の要素の配列（EffectSys の中のポインタの先。読み込みで CRT から取るので、塊とは別にコピーする）。要素の数は
/// `(count + extra + 1) * 0xD8` バイト（`0x0040D398 lea 0x1(%edi,%eax,1),%edi` / `0x0040D39C imul $0xd8`）。
pub const ROLLBACK_EFFECT_ARRAYS: &[RbEffect] = &[
    RbEffect {
        name: "p1_effect_array",
        sys_ptr_at: 0x004A_7D9C,
        count: 0x100,
        extra: 7,
        source: site_source!("0x0041B25D push $0x7 / 0x0041B25F mov $0x100,%edx"),
    },
    RbEffect {
        name: "p2_effect_array",
        sys_ptr_at: 0x004A_7DD4,
        count: 0x100,
        extra: 7,
        source: site_source!("p1 と同じ呼び出し（陣のループ）"),
    },
    RbEffect {
        name: "effect_array_3",
        sys_ptr_at: 0x004A_7E0C,
        count: 0x320,
        extra: 1,
        source: site_source!("0x0041B2BF push %esi（= 1）/ 0x0041B2C3 mov $0x320,%edx"),
    },
];
/// EffectSys の中: 要素の配列のポインタ・数・足し分（`0x0040D3B9` / `0x0040D395` / `0x0040D3AD`）と要素の大きさ。
pub const EFFECT_ARRAY_PTR: u32 = 0x30;
pub const EFFECT_ARRAY_COUNT: u32 = 0x34;
pub const EFFECT_ARRAY_EXTRA: u32 = 0x38;
pub const EFFECT_ELEM_BYTES: u32 = 0xD8;

/// コピーする .data の範囲（ゲーム自身のコードだけが引く所）。除いたもの: CRT の初期化の表 `0x4A0000`〜`0x4A009C`、ライブラリ（D3DX・CRT）の .data `0x4A3000`〜`0x4A7B10` と
/// `0x4E28BC` から後、更新の列のレジストリ `0x4ACFC8`〜`0x4AD018`、ウィンドウと Supervisor `0x4B30B0`〜`0x4B38B0`、FPS の文字 `0x4B41B8`〜`0x4B42B8`、SoundManager から後 `0x4DC698`〜。
pub const ROLLBACK_DATA: &[RbData] = &[
    RbData {
        name: "data_tables",
        start: 0x004A_00C0,
        end: 0x004A_2200,
        source: site_source!(
            "ゲームが引く最初 0x4A00C4（0x00403F64）〜最後 0x4A21DC（0x00431973）"
        ),
    },
    RbData {
        name: "game_globals",
        start: 0x004A_7C80,
        end: 0x004A_CE0C,
        source: site_source!(
            "ゲームが引く最初 0x4A7C80（0x0040E881）。ライブラリが引く最後は 0x4A7B10"
        ),
    },
    RbData {
        name: "rng_input",
        start: 0x004A_CE0C,
        end: 0x004A_CFC8,
        source: site_source!(
            "乱数 0x4ACE0C・抽選回数 0x4ACE10・InputManager 3 本（0x4ACE18 + 3 × 0x8E）"
        ),
    },
    RbData {
        name: "globals_b",
        start: 0x004A_D018,
        end: 0x004B_30B0,
        source: site_source!("レジストリの後（0x4AD018）〜窓（0x4B30B0）の前"),
    },
    RbData {
        name: "static_vms",
        start: 0x004B_38B0,
        end: 0x004B_41B8,
        source: site_source!(
            "statics.json の STATIC_VM 0x4B38B0 / 0x4B3B54 / 0x4B3DF8 〜 FPS の文字の前"
        ),
    },
    RbData {
        name: "globals_c",
        start: 0x004B_42B8,
        end: 0x004C_E438,
        source: site_source!("FPS の文字（0x100）の後 〜 AsciiManager の更新の欄の前"),
    },
    RbData {
        name: "ascii",
        start: 0x004C_E438,
        end: 0x004D_C698,
        source: site_source!(
            "AsciiManager 0x4CE458 と更新・描画の欄・AnmManager* 0x4DC550 〜 SoundManager の前"
        ),
    },
];

/// 比べない所（コピーと書き戻しはする。D1 と D2 の比べからだけ外す）。どれも同期と関係ない表示・実時間の値（fps）。**外す所の原本はこの表 1 つ**。粒度は fps の欄だけ
/// （粗く外すと本物のずれを隠す）。`ascii_pen_x` と `sprite_temp` は同じフレームの描画の中でだけ書く・読む場所。表は 5 行（`MAX_SKIP_ROWS` = 8 に収まる）。
pub const ROLLBACK_COMPARE_SKIPS: &[RbSkip] = &[
    // fps の計測 `0x4316B0`（描画の優先度 31 のノード `0x431B30` から）だけが書く 6 語（timeGetTime の値・描画の回数・500 ms ごとの計算し直し）。
    // `0x4b42b8`〜`0x4b42cf` を即値で引く 22 行は全部 `0x4316C2`〜`0x4317F3` の中。6 語とも fps の計測の欄なので、これより細かくは割れない。
    // 終わりの `0x4B42D0` は NetworkManager へのポインタなので含めない。
    RbSkip {
        name: "fps_timing",
        kind: RbSkipKind::Range {
            start: FPS_TIMING_START,
            end: FPS_TIMING_END,
        },
        source: site_source!("0x004316C2〜0x004317F3（fps の計測 0x4316B0）だけが引く"),
    },
    // AsciiManager（`0x4CE458`）の文字の列: `+0x2264` から 0x60 バイト × 0x100 本、数は `+0x8264`（`0x004342A3`〜`0x004342C0`）。1 本は文字 `+0x00`〜・位置の 3 語 `+0x40`（`0x004342DF lea 0x40(%edx)`）・色など。
    // 列には fps のほかに数字の文字も積まれる（Gui の `%7d` など。`0x00417FA2`〜`0x00418184`）ので、列を丸ごとは外さない。fps の文字は `0x0043191D` だけが積み、位置は (280.0, 466.0, 0.0)
    // （`0x004318FD push $0x43e90000` / `0x00431902 push $0x438c0000`）。y の 466.0 は exe の中でここだけ。枠の番号は前に積まれた数で変わるので、位置の欄で見分ける。
    // 外すのは、D1 と D2 の両方で位置の欄が fps の位置の枠の文字の欄 0x40 バイトだけ（位置・色・数の欄 `0x4D66BC` とほかの枠は比べる）。
    RbSkip {
        name: "fps_text",
        kind: RbSkipKind::TextAt {
            list: 0x004D_06BC,
            stride: 0x60,
            count: 0x100,
            text: 0x40,
            pos_off: 0x40,
            pos: [0x438C_0000, 0x43E9_0000, 0],
        },
        source: site_source!(
            "0x0043191D（fps の文字）/ 0x004342B9 lea 0x2264(%edx,%ecx,1) / 0x004342DF lea 0x40(%edx)"
        ),
    },
    // 記録の処理 `0x4202A0`（優先度 26。this は ReplayManager）が、30 tick に 1 回描画の fps `[0x4B36C8]` の下位 1 バイトを書き位置 `[RM+0x18]` へ書いて 1 進める（`0x00420348`〜`0x00420352`）。
    // 書き先の塊（0x79 バイト）は fps の欄だけの並びで、取り直しは `0x004203E3 push $0x79` → `0x004203ED call 0x401340`（戻りアドレス `0x004203F2`）、最初の塊は `0x0042154E push $0x79`（戻りアドレス `0x0042155D`。読み込みの側で取るなら DLL の領域に入らず、この札は使われない。推定）。
    // 外すのは、札がこの 2 つの塊のうち、書き位置が S の先頭から D1・D2 の先頭までに進んだバイトだけ（期間 k tick で ceil(k / 30) バイト前後）。書き位置の前（過去の fps）と後（0 のまま）は比べる。
    // `RM+0x18` そのものと 0x1C20 × 3 の塊（入力の記録）も比べる。
    RbSkip {
        name: "replay_fps",
        kind: RbSkipKind::Cursor {
            callers: &[0x0042_03F2, 0x0042_155D],
            ptr_at: REPLAY_MANAGER_PTR,
            cursor_off: 0x18,
        },
        source: site_source!(
            "0x00420348 mov 0x4b36c8,%al / 0x00420350 mov %al,(%edx) / 0x00420352 incl 0x18(%esi)"
        ),
    },
    // 実機の比べの唯一の違いの正体その 1: AsciiManager（`0x4CE458`）の `vm_0.pos.x`（`+0x208` = `0x4CE660`）。文字の列を描く `0x434540` が `0x43458E` で書き、
    // `0x434643`〜`0x434648` で 1 文字ごとに 9 × 拡大だけ進める。同じフレームの描画の中でだけ読む。
    RbSkip {
        name: "ascii_pen_x",
        kind: RbSkipKind::Range {
            start: 0x004C_E660,
            end: 0x004C_E664,
        },
        source: site_source!(
            "0x43458E（AsciiManager+0x208 に書く）/ 0x434643〜0x434648（1 文字ごとに 9×拡大だけ進める）"
        ),
    },
    // AsciiManager の `__vm_1`（`+0x2A4`〜`+0x548`、0x2A4 B = `0x4CE6FC`〜`0x4CE9A0`）。点数の浮き文字を描く作業用の VM で、書くのはリセット `0x4343E0` と描画 `0x4353F0`
    // （`0x43547D fstps 0x4b0(%esi)` = pos.y は exe で唯一の `0x4b0(` の書き）だけ、読むのは同じ描画の `0x4355A2 lea 0x2a4(%esi)` → `0x43A950` だけ。
    // 描画は生きている浮き文字が無いフレームでは書かないので、期間の終わりに浮き文字が消えていると進め直した側に古い値が残る。更新の状態が入っていない VM なので、丸ごと外しても
    // 本物のずれは隠さない（浮き文字の配列 `+0xB240`〜は比べ続ける）。
    RbSkip {
        name: "ascii_vm1",
        kind: RbSkipKind::Range {
            start: 0x004C_E6FC,
            end: 0x004C_E9A0,
        },
        source: site_source!(
            "0x4343E0（リセット）と 0x4353F0（描画。0x43547D fstps 0x4b0(%esi)）だけが書く / 0x4355A2 → 0x43A950 だけが読む"
        ),
    },
    // 正体その 2: AnmManager の SPRITE_TEMP_BUFFER（`zRenderVertex144[4]`、0x1C × 4 = 0x70 B、`0x4DC558`）。`0x43A950` が `0x43A9B8`〜`0x43AA3D` で
    // 頂点を置く、描画用だけの作業場。
    RbSkip {
        name: "sprite_temp",
        kind: RbSkipKind::Range {
            start: 0x004D_C558,
            end: 0x004D_C5C8,
        },
        source: site_source!(
            "0x43A950（AnmManager の SPRITE_TEMP_BUFFER）/ 0x43A9B8〜0x43AA3D（頂点を置く）"
        ),
    },
];

// ---- 描画用の VM の欄の診断と除外 ----
// 描画の列が書く「ANM の VM の描画用の欄」（回転 z と画面座標 pos）を、弾・敵・自機ショットの枠と arena の VM の配列について、違ったバイトを（範囲・枠・欄・D1 と D2 での生死）に分けて数え、
// `skip_draw_fields = dead` のとき D1 の時点で死んでいる枠の描画用の欄だけを比べから外す。検査 `slot_values_appear_in_the_note`。

/// ANM の VM 1 本の大きさ（`0x0040CB01 imul $0x2a4,%eax,%eax`（エフェクトの VM の配列。戻りアドレス `0x0040CB17`）・
/// `0x00440D9D imul $0x2a4,%ebx,%ebx`（Ex の配列。戻りアドレス `0x00440DB9`））。
pub const VM_BYTES: u32 = 0x2A4;
/// VM の回転 z（4 バイト）: 描画が `0x0040F840`（`0x0040F846 mov %eax,0x8(%ecx)` / `0x0040F849 orl $0x4,0x1f8(%ecx)`）で書く。
/// 弾 `0x00413BB1 call 0x40f840`（ecx = VM）・敵 `0x0041176C lea 0x8(%esi),%ecx` → `call 0x40f840`・自機ショット `0x0041F6C1`
/// `call 0x40f840`（ecx = 枠の先頭 = VM）。
pub const VM_ROT_Z_OFF: u32 = 0x8;
pub const VM_ROT_Z_LEN: u32 = 4;
/// VM の画面座標 pos（x, y, z の float 3 つ = 12 バイト）: 弾 `0x00413B3A lea 0x208(%esi),%ecx`（`0x00413B5A fstps (%edi)` /
/// `0x00413B6D fstps 0x4(%edi)` / `0x00413B70 movl $0x3d4ccccd,0x8(%edi)`）・敵 `0x004116FA lea 0x208(%ebx),%edi` と
/// `0x00411789 lea 0x210(%esi),%edi`（= VM `+0x8` の pos）・自機ショット `0x0041F6D7 fstps 0x208(%esi)` / `0x0041F6EE` / `0x0041F6FC`。
pub const VM_POS_OFF: u32 = 0x208;
pub const VM_POS_LEN: u32 = 12;
/// VM の補間の始点（`interp_data` の先頭 `pos_initial`、float 3 つ = 12 バイト）: 補間を始める命令 17 / 18 / 19 / 32 が、`+0x1F8` の bit 0x100 が 0 なら pos `+0x208`（`0x43768C lea 0x208(%edi)`）を、
/// 1 なら `+0x288` を `+0x238` へコピーする。pos は描画が毎フレーム絶対値で書く欄なので、進め直しの期間の中で描かれなかった VM は元の実行と違う pos を持ち、始点も違う値になる。
/// 読み手は補間 `0x438FDB`〜`0x439072` だけで、世界の欄へ流れる読み手は見つからなかった。外すのは `+0x238`〜`+0x243` だけで、手前 `+0x228`〜`+0x237`（割り込みのバックアップ）と終点 `+0x244`〜は外さない。
pub const VM_INTERP_START_OFF: u32 = 0x238;
pub const VM_INTERP_START_LEN: u32 = 12;

/// arena の中で VM の配列を取る確保の札（呼び出し元の戻りアドレス）: `0x0040CB17`（エフェクト生成 `0x40CA60`。`n × 0x2A4` バイト）・
/// `0x00440DB9`（Ex。`n × 0x2A4 + 追加` バイト。`0x00440D90`）。空きの塊は札を失う（割り当て器の先頭で札と「列の次」が同じ語）ので、空きは大きさ（`0x2A4` の倍数）で推す。
pub const ROLLBACK_ARENA_VM_TAGS: &[u32] = &[0x0040_CB17, 0x0044_0DB9];
/// Ex の配列の札（`ROLLBACK_ARENA_VM_TAGS` の 2 つ目。`n × 0x2A4 + 追加` バイトなので、VM の本数は大きさの floor では出ない）。
pub const ROLLBACK_ARENA_EX_TAG: u32 = 0x0044_0DB9;
/// Ex の配列（`0x00440D90(n, 追加)`）の（求められた大きさ, VM の本数 n）。`0x00440D90` は `n × 0x2A4 + 追加` を確保し、全体を `0x00440DC9 rep stos` で 0 にして、
/// `base + n × 0x2A4` を状態ブロックとして `+0x34` に置く。追加が 0x2A4 以上だと、大きさの floor は VM の本数より多く、状態ブロックを「幻の VM」に数えてしまう。
/// 呼び出し元 25 か所の全部の組: 追加 0x4C・0x50・0x58・0x5C（`0x440E4D` `0x440FBD` `0x44191D` `0x443448` `0x4455DD` `0x44BCDD` `0x44C35D` / `0x44772D` `0x447AFD` `0x448329` / `0x444F09` `0x445059` /
/// `0x445ECD` `0x44632D` `0x4467CD` `0x446C1D` `0x446D89` `0x44AC2D`）はどれも n = 1。ほか: 追加 0x190・n = 5（`0x442592`）、追加 0x158・n = 4（`0x449080` `0x449800`）、追加 0xF48（`0x443C80` `0x444572`）、
/// 追加 0x688（`0x44A042`）、追加 0x68C（`0x44B232`）。表に無い大きさは本数 0（知らない塊は隠さない向き）。
pub const ROLLBACK_EX_VM_COUNTS: &[(u32, u32)] = &[
    (0x2F0, 1),
    (0x2F4, 1),
    (0x2FC, 1),
    (0x300, 1),
    (0xEC4, 5),
    (0xBE8, 4),
    (0x11EC, 1),
    (0x92C, 1),
    (0x930, 1),
];

/// Ex の配列（札 `ROLLBACK_ARENA_EX_TAG`）の、求められた大きさ `size` に対する VM の本数（表に無ければ 0）。
pub fn ex_vm_count(size: u32) -> u32 {
    ROLLBACK_EX_VM_COUNTS
        .iter()
        .find(|&&(s, _)| s == size)
        .map_or(0, |&(_, n)| n)
}

/// レーザーの配列: 弾の塊の中の先頭・1 本の大きさ・本数・生死の旗 u32 の位置。
pub const LASER_BASE: u32 = 0x24D424;
pub const LASER_STRIDE: u32 = 0x59C;
pub const LASER_COUNT: u32 = 48;
pub const LASER_ALIVE: u32 = 0x584;
/// レーザー 1 本の中の、VM の回転 z・pos のほかに、描画 `0x413BE0` だけが書く欄（位置, 長さ。小さい順）: VM 1 の拡大 `+0x2BC`〜`+0x2C3`（`0x00413D94 fsts 0xb4(%esi)`。esi = laser + 0x208）、
/// VM 1 の色と α `+0x494`〜`+0x497`（`0x00413D6A mov %eax,0x28c(%esi)`）、`+0x596`（u16。`0x00413CAD movw $0xffff,0x38e(%esi)`。生成 `0x0041335F` も書く）。
/// 旗 `+0x49C` は**低位 1 バイトだけ**入れる: 生成で VM 1 を初期化する `0x004127B0` の `0x004127E0 movw $0x7,0x1f8(%esi)` で 7、生成 `0x00413330`〜`0x0041333C` で 0x17、描画 `0x00413D76`〜`0x00413D7F`（`or $0x40`）で 0x57。
/// 期間の中で生まれたレーザーは描かれた側 0x57・進め直し側（描かない）0x17 で低位 1 B が必ず違う。上位 3 バイト `+0x49D`〜`+0x49F` は外さない。
/// VM 0 の回転 z `+0x8` は描画でなく更新が書く（`0x00414C40 call 0x40F840`。値は `+0x554` から決まり、`+0x554` は比べ続ける）ので、`all` で外すのは過剰な除外だが、本物のずれは `+0x554` で落ちる。
/// **入れないもの**: 旗 `+0x49D`〜`+0x49F`・世界の欄・生死 `+0x584`。
pub const LASER_DRAW_ONLY: &[(u32, u32)] = &[(0x2BC, 8), (0x494, 4), (0x49C, 1), (0x596, 2)];
/// レーザー 1 本の世界の欄（診断が別に数える。外さない）: 始点 `+0x548`・角度 `+0x554`・尾 `+0x558`・頭 `+0x55C`・当たり判定の欄・生死 `+0x584` から `+0x599` まで
/// （当たり判定 `0x00414BCB`〜`0x00414C11` が `+0x548`・`+0x554`〜`+0x564` を読む）。描画だけの欄の `+0x596`〜`+0x597` は `LASER_DRAW_ONLY` に数える（診断は描画だけの欄を先に引く）。
pub const LASER_WORLD: &[(u32, u32)] = &[(0x548, 0x52)];

/// 枠の配列 8 本（弾・レーザー・敵・自機ショットの 1P と 2P）。範囲の名前は `ROLLBACK_BLOCKS` と同じ。**同じ範囲を名指す枠の配列は、この表で `base` の小さい順に、
/// 占める所が重ならないように並べる**（弾とレーザー。`compare_region_slots` が前提にする。検査 `slot_table_is_consistent`）。
pub const ROLLBACK_SLOT_ARRAYS: &[RbSlotArray] = &[
    // 弾: `BulletManager+0x1A900`・stride 0x10C4・on_tick が回すのは 0〜535 の 536 枠。生きているのは `+0xDBE`（u16）が 0 でも 6 でもないとき
    // （読み手 `0x00413AF4 movzwl 0xdbe(%ebx)`）。描く `0x413AF0` は状態 2〜5 のとき VM 1〜4、それ以外は VM 0 を選ぶ（`0x00413B0B lea 0x2a8(%ebx)` /
    // `0x00413B13 lea 0x54c` / `0x00413B1B lea 0x7f0` / `0x00413B23 lea 0xa94` / `0x00413B2B lea 0x4(%ebx)`）ので、VM は 5 本。
    RbSlotArray {
        label: "p1_bullets",
        region: "p1_bullets",
        base: 0x1A900,
        stride: 0x10C4,
        count: 536,
        alive: AliveRule::U16NotIn {
            off: 0xDBE,
            dead: &[0, 6],
        },
        vms: &[0x4, 0x2A8, 0x54C, 0x7F0, 0xA94],
        probe_vms: &[],
        draw_only: &[],
        world: &[],
        pos_by_vm: false,
        source: site_source!("Memory_Map 弾配列注3・注7 / 0x00413AF0〜0x00413B70"),
    },
    RbSlotArray {
        label: "p2_bullets",
        region: "p2_bullets",
        base: 0x1A900,
        stride: 0x10C4,
        count: 536,
        alive: AliveRule::U16NotIn {
            off: 0xDBE,
            dead: &[0, 6],
        },
        vms: &[0x4, 0x2A8, 0x54C, 0x7F0, 0xA94],
        probe_vms: &[],
        draw_only: &[],
        world: &[],
        pos_by_vm: false,
        source: site_source!("p1_bullets と同じ（陣ごとの塊）"),
    },
    // レーザー（弾の塊 `p?_bullets` の中で、弾の枠の後ろ）: `BulletManager+0x24D424`・stride 0x59C・48 本（描く `0x413BE0` の `0x00413BFF add $0x24d424,%esi` / `0x00413C0E movl $0x30,-0x14(%ebp)` /
    // `0x00413DDB add $0x59c`、構築は `0x004150A0 push $0x30` / `0x004150A2 push $0x59c`）。生死は u32 `+0x584` の bit0（描く側 `0x00413C20 mov 0x37c(%esi),%eax`（esi = laser + 0x208）→ `0x00413C28 je 0x413dcf`、
    // 更新側 `0x00414B60 mov 0x584(%esi),%eax`）。VM は `+0x0` と `+0x2A4`。VM 0 の pos は `+0x208`、VM 1 の pos は `+0x4AC`（`0x00413D06 lea 0x2a4(%esi),%ecx` → `0x00413D0C call 0x4343d0`）。
    // 弾は 536 枠 = `[0x1A900, 0x24C360)`、レーザーは `[0x24D424, 0x25E164)`。間の `[0x24C360, 0x24D424)`（構築 `0x00415074`〜`0x00415096` の 176 ＋ 361 = 537 枠のうち最後の 1 枠分）と
    // 後ろの `[0x25E164, 0x25E1C0)` は、どちらの配列の枠でもない（`p?_bullets` の行の「枠の外」に数える）。
    RbSlotArray {
        label: "p1_lasers",
        region: "p1_bullets",
        base: LASER_BASE,
        stride: LASER_STRIDE,
        count: LASER_COUNT,
        alive: AliveRule::U32Bit0 { off: LASER_ALIVE },
        vms: &[0x0, 0x2A4],
        probe_vms: &[],
        draw_only: LASER_DRAW_ONLY,
        world: LASER_WORLD,
        pos_by_vm: true,
        source: site_source!(
            "ノート §23 / 0x00413BE0〜0x00413DE8 / 0x00414B4D / 0x00414F40 / 0x004150A2"
        ),
    },
    RbSlotArray {
        label: "p2_lasers",
        region: "p2_bullets",
        base: LASER_BASE,
        stride: LASER_STRIDE,
        count: LASER_COUNT,
        alive: AliveRule::U32Bit0 { off: LASER_ALIVE },
        vms: &[0x0, 0x2A4],
        probe_vms: &[],
        draw_only: LASER_DRAW_ONLY,
        world: LASER_WORLD,
        pos_by_vm: true,
        source: site_source!("p1_lasers と同じ（陣ごとの塊）"),
    },
    // 敵: `+0x5758`・stride 0x5430・128 体（`0x0040F1FF cmp $0x80,%eax`）。生きているのは `+0x337C`（u32 の旗）の bit0（生死の判定は座標でなくこれ）。VM は `+0x8`（`0x00411769 lea 0x8(%esi),%ecx`）。
    // 描く `0x411670` は、もう 1 本の VM `+0x2AC`（`0x004116A8 lea 0x2ac(%esi),%ebx`）にも同じ形で回転 z・pos を書く。測定がこの VM を名指ししていない（未確認）ので、除外に入れず診断だけで数える（`probe_vms`）。
    RbSlotArray {
        label: "p1_enemies",
        region: "p1_enemies",
        base: 0x5758,
        stride: 0x5430,
        count: 128,
        alive: AliveRule::U32Bit0 { off: 0x337C },
        vms: &[0x8],
        probe_vms: &[0x2AC],
        draw_only: &[],
        world: &[],
        pos_by_vm: false,
        source: site_source!("Memory_Map zEnemy 注2 / 0x00411670〜0x00411789"),
    },
    RbSlotArray {
        label: "p2_enemies",
        region: "p2_enemies",
        base: 0x5758,
        stride: 0x5430,
        count: 128,
        alive: AliveRule::U32Bit0 { off: 0x337C },
        vms: &[0x8],
        probe_vms: &[0x2AC],
        draw_only: &[],
        world: &[],
        pos_by_vm: false,
        source: site_source!("p1_enemies と同じ（陣ごとの塊）"),
    },
    // 自機ショット（自機の塊 `p?_player` の中）: `+0xC11C`・stride 0x484・128 枠（`0x0041F689 lea 0xc11c(%edi),%esi` / `0x0041F68F movl $0x80,-0x4(%ebp)` / `0x0041F73C add $0x484,%esi`）。
    // 状態は `+0x462`（u16）: 0 が空き（出す側 `0x0041F4F0 cmpw $0x0,0x462(%esi)` → 空きなら 1 を置く `0x0041F525`。更新 `0x0041F5B0` は空きを飛ばす）、1 と 2 が使用中
    // （描画は `0x0041F6A0 cmpw $0x1` と `0x0041F770 cmpw $0x2`。2 を置くのは `0x0041FE5A mov %si,0x462(%edi)`）。VM は枠の先頭（`0x0041F6D7 fstps 0x208(%esi)`）。世界座標は `+0x2A4`（`0x0041F6C6`。これは比べる）。
    RbSlotArray {
        label: "p1_player_shots",
        region: "p1_player",
        base: 0xC11C,
        stride: 0x484,
        count: 128,
        alive: AliveRule::U16NotIn {
            off: 0x462,
            dead: &[0],
        },
        vms: &[0x0],
        probe_vms: &[],
        draw_only: &[],
        world: &[],
        pos_by_vm: false,
        source: site_source!(
            "0x0041F689〜0x0041F73C / 0x0041F4F0 / 0x0041F5B0 / 0x0041F6A0 / 0x0041F770"
        ),
    },
    RbSlotArray {
        label: "p2_player_shots",
        region: "p2_player",
        base: 0xC11C,
        stride: 0x484,
        count: 128,
        alive: AliveRule::U16NotIn {
            off: 0x462,
            dead: &[0],
        },
        vms: &[0x0],
        probe_vms: &[],
        draw_only: &[],
        world: &[],
        pos_by_vm: false,
        source: site_source!("p1_player_shots と同じ（陣ごとの塊）"),
    },
];

/// ゲーム自身のコード（戻りアドレスがここにある確保だけを DLL の領域へ向ける）。`0x0044E5A0` はゲームのコードが
/// 呼ぶライブラリの最初の関数（import の `jmp *0x48e010`）で、そこから後は D3DX・CRT。
pub const GAME_CODE_START: u32 = 0x0040_1000;
pub const GAME_CODE_END: u32 = 0x0044_E5A0;
/// 確保の包み `0x00401340` の中の `call 0x47B151` の戻りアドレス。これが呼び出し元なら、包みを呼んだ所（malloc の入口で `[esp + 12]`: 戻りアドレス・大きさ・
/// 包みの `push ebp` の上）を呼び出し元と見る。
pub const ALLOC_WRAP_RET: u32 = 0x0040_134C;
pub const ALLOC_WRAP_OUTER: u32 = 12;
/// ゲームのコードの中でも、DLL の領域へ向けない呼び出し元（状態の外）。
pub const ROLLBACK_EXCLUDED_CALLERS: &[RbCallers] = &[
    RbCallers {
        start: 0x0043_BE50,
        end: 0x0043_CCC6,
        // 範囲の中の確保 0x43C6E9 / 0x43C715（call 0x401340）は ANM の読み込み 0x43C610（preload_anm 0x43C7A0 と AnmManager の構築 0x43CFA0 から。読み込みのときだけ）。
        // 文字の絵の関数 0x43BE50 もこの範囲にあるが、その確保は D3DX の中（0x4524AE から先。GAME_CODE_END の外）なので、領域の外に残るのはこの除外でなく
        // GAME_CODE_END の境界による。
        why: "ANM の読み込み（0x43C610。0x43C6E9 / 0x43C715 の確保）と、文字の絵の関数 0x43BE50 を含む範囲",
    },
    RbCallers {
        start: 0x0043_F890,
        end: 0x0043_F9E6,
        why: "BGM のストリームの通知の配列（DirectSound。0x43F8EC / 0x43F94C の new）",
    },
    // ゲームのスクリーンショット `0x42F540` は確保の包み `0x401340` を 2 回呼ぶ（`0x42F5D2` 0x2C B・`0x42F60E` 0xE1000 B）。転送が有効な間は DLL の領域へ入り、
    // 片側の PC だけ領域の top と空きの並びが変わる（P/Home の 0x800 は試合の語のマスクの外なので相手へは送らない。自分の側では `shot_key_win` が
    // Present のラッパを呼ぶ間だけ立てて撮る）。終わりは `0x0042F75C ret $0x4`（3 B）の次。
    RbCallers {
        start: 0x0042_F540,
        end: 0x0042_F75F,
        why: "ゲームのスクリーンショット（0x42F5D2 / 0x42F60E の確保。片側の PC だけで起きる）",
    },
];

/// 更新の列のレジストリ（`0x0042DCA4 mov $0x4acfc8,%ecx` → tick、`+0x20` が描画の列 —— `0x0042C7EB lea 0x20(%ecx),%esi`）。
/// 節の次は `+0x14`（`0x0042C77D`）、更新の関数は `+0x04`（`0x0042C73A`）、this は `+0x1C`（`0x0042C737`）。
pub const UPDATE_REGISTRY: u32 = 0x004A_CFC8;
pub const UPDATE_DRAW_HEAD: u32 = 0x20;
pub const UPDATE_NEXT: u32 = 0x14;
pub const UPDATE_FN: u32 = 0x04;
pub const UPDATE_ARG: u32 = 0x1C;

/// ScreenEffect の本体の大きさ（`0x422D2A push $0x38` → operator new `0x422D35`。`0x422D6B`〜`0x422D72` の `rep stos` も 14 語 = 0x38 B を 0 で埋める）。
/// 作るのは `0x422D20` だけで、呼び手は 6 か所（`0x416E53` / `0x417A51` / `0x417A6F` / `0x41ABED` / `0x424BA8` / `0x430521`。どれも `call 0x422d20`）。
pub const SCREEN_EFFECT_BYTES: u32 = 0x38;
/// 本体の中の、更新の節・描画の節への戻りの欄（`0x422ED1 mov %esi,0x8(%ebx)` / `0x422ECE mov %edi,0xc(%ebx)`。種類 1・7 は描画の節が無く `+0x0C` は 0）。
/// 後片付け `0x422900` は更新の節の `+0x0C` を 0 にしてから `0x42C8C0([本体+0x0C])` で描画の節を外し、本体の `+0x0C` を 0 にして本体を解放する
/// （`0x422906` / `0x422916` / `0x42291C` / `0x422923`）。
pub const SCREEN_EFFECT_UPDATE_NODE: u32 = 0x08;
pub const SCREEN_EFFECT_DRAW_NODE: u32 = 0x0C;

/// ScreenEffect の種類 1 つの、更新の関数と描画の関数（節の `+0x04`。`draw == 0` は描画の節を作らない種類）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenEffectKind {
    pub kind: u8,
    pub update: u32,
    pub draw: u32,
}

/// ScreenEffect の種類の表。`0x422D85 jmp *0x422ee0(,%edi,4)` の 8 つ組（`0x422EE0` の 4 バイト × 8）が指す枝 `0x422D8C` / `0x422D93` / `0x422D9D` / `0x422DED` / `0x422DC5` / `0x422E12` /
/// `0x422E37` / `0x422E5C` が、更新の関数（`0x42B300` へ push）と描画の関数（同じく push。種類 1・7 は無し）を決める。描画の関数は `th09_draw_and_renderonly_M1c.md` の表と一致
/// （検査 `screen_effect_draws_appear_in_the_m1c_note`）。上の 11 個の関数のアドレスは、`.text` では `0x422D20` の中の `push $imm32` だけが参照する（`.rdata` の表は未確認）。
pub const SCREEN_EFFECT_KINDS: [ScreenEffectKind; 8] = [
    ScreenEffectKind {
        kind: 0,
        update: 0x0042_22C0,
        draw: 0x0042_2B70,
    },
    ScreenEffectKind {
        kind: 1,
        update: 0x0042_2660,
        draw: 0,
    },
    ScreenEffectKind {
        kind: 2,
        update: 0x0042_2470,
        draw: 0x0042_2BD0,
    },
    ScreenEffectKind {
        kind: 3,
        update: 0x0042_25B0,
        draw: 0x0042_2CD0,
    },
    ScreenEffectKind {
        kind: 4,
        update: 0x0042_2470,
        draw: 0x0042_2B70,
    },
    ScreenEffectKind {
        kind: 5,
        update: 0x0042_24F0,
        draw: 0x0042_2C10,
    },
    ScreenEffectKind {
        kind: 6,
        update: 0x0042_24F0,
        draw: 0x0042_2C50,
    },
    ScreenEffectKind {
        kind: 7,
        update: 0x0042_27A0,
        draw: 0,
    },
];

/// 節の更新・描画の関数 `f`（節の `+0x04`）が ScreenEffect のものか。
pub fn is_screen_effect_fn(f: u32) -> bool {
    f != 0
        && SCREEN_EFFECT_KINDS
            .iter()
            .any(|k| k.update == f || k.draw == f)
}

// ---- 1 回の描画の中で更新の走査を n + 1 回回す ----
// 地点の表 `SITES`（14 行）・`PACING_SITES` とは別に持つ（`SITES` はどの実行でも全行の一致が設置の条件なので、ここへ足すと使わない実行の設置の条件が変わる）。
// 書くのは ini の `[rollback_probe]` に `resim = frame` があるときだけ（`rollback_win::arm`）。値の転記ミスは検査 `resim_values_appear_in_the_note` が見る。

/// do_frame の走査の call の行の名前。
pub const RESIM_SCAN_SITE: &str = "resim_scan_call";
/// 地点の表（1 行）。do_frame `0x0042DC40` の中の `0x0042DCA9 call 0x42c700`（exe で唯一の呼び出し）。`0x0042DCA4 mov $0x4acfc8,%ecx` の直後で、
/// 戻り値は `0x0042DCB3 mov %eax,%edi` が受け、`0x0042DCBA test %edi,%edi` → 0 なら `0x0042DCC3 call 0x430230` で 1 を返し、−1 なら 2 を返し、
/// それ以外は描画へ進む。差し替え先は走査を包み、普段は 1 回呼んで戻り値をそのまま返す。
pub const RESIM_SITES: &[SiteDef] = &[SiteDef {
    name: RESIM_SCAN_SITE,
    addr: 0x0042_DCA9,
    expected: &[0xE8, 0x52, 0xEA, 0xFF, 0xFF],
    kind: Use::CallRel32 {
        original_target: 0x0042_C700,
    },
    source: site_source!("ノート §15（th09_disasm.txt L58470〜L58474）"),
}];

/// SoundManager（`0x0042DCAE mov $0x4dc698,%ecx` → `0x0042DCB5 call 0x43f1f0`）。コピーの範囲の外（`ROLLBACK_DATA` の `ascii` は `0x004DC698` の前まで）で、
/// 進め直しの最後の回の前に下の欄を戻す。
pub const SOUND_MANAGER: u32 = 0x004D_C698;
/// SE の待ち行列: id の 12 枠（i32。−1 が空）の始まりと、枠ごとの回数（i32）の始まり。積む `0x43E2F0` / `0x43E380` は `lea 0x620(%ecx)` で枠を探し、空なら id を `0x620(%ecx,%eax,4)` へ書き
/// `incl 0x650(%ecx,%eax,4)`（`0x0043E36E`）。鳴らす `0x43F1F0` の後半（`0x0043F660`〜`0x0043F68B`）は枠を読んで id を `movl $-1`、回数を `movl $0` にする。枠の数は `cmp $0xc`（`0x0043E31E`）。
/// 枠が空かどうかは id だけで決まるので、id を −1・回数を 0 にすれば積んだ SE は消える（パンの欄 `+0x680` は回数の分しか読まれない）。
pub const SND_SE_IDS: u32 = 0x620;
pub const SND_SE_COUNTS: u32 = 0x650;
pub const SND_SE_SLOTS: u32 = 12;
/// SE のパンの欄: 枠 `s` の `k` 個目は `+0x680 + s × 0x200 + k × 4`（積む `0x43E2F0` の `0x0043E33B shl $0x7` / `mov %edx,0x680(%ecx,%esi,4)` と、空き枠への `0x0043E35D shl $0x9`）。
/// 回数は `cmp $0x80`（`0x0043E331`）で 0x80 未満のときだけ増えるので、1 枠のパンは 0x80 個まで（0x200 バイト）。鳴らす `0x43F1F0` は回数個を足して回数で割った平均を使う（`0x0043F689 idivl (%edi)`）。
pub const SND_SE_PANS: u32 = 0x680;
pub const SND_SE_PAN_SLOT_BYTES: u32 = 0x200;
pub const SND_SE_MAX_COUNT: u32 = 0x80;
/// SE を積む `0x43E2F0`（thiscall、ecx = `SOUND_MANAGER`、スタックに (id, パン)、`ret $0x8`）の入口と、先頭 25 バイト（`push %ebp`〜`lea 0x620(%ecx),%edx`。`0x0043E2F0`〜`0x0043E308`）。
/// 進め直しで初めて起きた SE を遅れて積むとき、呼ぶ前に `th09.exe` の実バイトと照合する（合わなければ呼ばずに数える）。呼ぶ約束はゲーム自身の呼び出し
/// `0x00403FBC push $0x35`・`0x0040407F push $0xe` → `0x00404081 mov $0x4dc698,%ecx` → `0x00404086 call 0x43e2f0` と同じ。副作用は、枠に無い id のとき `+0x408 + id × 4` へ表 `0x4A1F86 + id × 8` の値（定数）を書くだけ。
pub const SE_PUSH_FN: u32 = 0x0043_E2F0;
pub const SE_PUSH_HEAD: [u8; 25] = [
    0x55, 0x8B, 0xEC, 0x53, 0x56, 0x57, 0x8B, 0x7D, 0x08, 0x0F, 0xBF, 0x1C, 0xFD, 0x86, 0x1F, 0x4A,
    0x00, 0x33, 0xC0, 0x8D, 0x91, 0x20, 0x06, 0x00, 0x00,
];
/// カットインの SE の id（announce_attack `0x403E50`）。通常は `0x0040407F push $0xe`、種類 2 は `0x00403FBC push $0x35`。進め直しで
/// 初めて出た数を本ログで id 別に見るために名前を付ける。
pub const SE_ID_CUTIN: u32 = 0x0E;
pub const SE_ID_CUTIN_SPELL: u32 = 0x35;
/// 選択の画面のカーソルの移動の SE の id とパン。カーソルを ±1 する `0x0042474A` の `0x004247D6 push $0x0`・`0x004247D8 push $0xc` →
/// `0x004247DA mov $0x4dc698,%ecx` → `0x004247DF call 0x43e2f0`。キャラ選択で相手の席のカーソルを位置で書いたとき、同じ呼び出しで鳴らす
/// （「ランダム」の道 `0x00428B48`〜 / `0x00428CC6`〜 は鳴らさない）。
pub const SE_ID_MENU_CURSOR: u32 = 0x0C;
pub const SE_PAN_MENU_CURSOR: i32 = 0;
/// BGM の命令の待ち行列: 0x10C バイトの枠 × 31（`0x43EE40` の `cmpl $0x0,(%edx)`・`add $0x10c,%edx`・`cmp $0x1f,%eax`。`0x0043EE45 lea 0x1f88(%ecx)`・
/// `0x0043EE50`〜`0x0043EE5F`）。枠の先頭の語が 0 なら空き。鳴らす `0x43F1F0` が詰め直す（`0x0043F51D mov $0x43,%ecx` の `rep movsl` = 0x10C バイト）。
/// 積む側は走査の中、詰める側は走査の外なので、進め直しの前にバックアップして最後の回の前に戻す。
pub const SND_BGM_QUEUE: u32 = 0x1F88;
pub const SND_BGM_SLOT_BYTES: u32 = 0x10C;
pub const SND_BGM_SLOTS: u32 = 31;
/// AsciiManager（`0x4CE458`）の文字の数（`+0x8264`）。足す `0x4342A0`（`0x004342A3 mov 0x8264(%ecx)` / `0x004342C0` で書き戻し。256 本を超えた分は
/// `0x004342A9 cmp $0x100` → `jge` で捨てる）、描く `0x434540` の後の `0x00431611 call 0x430220`（`movl $0x0,0x8264(%ecx)`）で 0 に戻る。コピーの範囲（`ascii`）の中にある。
pub const ASCII_COUNT: u32 = 0x004D_66BC;

// ---- rollback のモードの製品の経路の部品 ----
// E1 の表（`ALLOC_SITES`・`ROLLBACK_DATA`）は変えず、足す分だけをここに置く（E1 の実験の入口の挙動を変えないため）。
// 値の転記ミスは `rollback_play` の検査 `play_values_appear_in_the_note` が見る。

/// calloc の行の名前。
pub const CALLOC_SITE: &str = "crt_calloc";
/// calloc と見られる `0x0047DE66`（SEH の前置き `push $0x10`; `push $0x495640` の後に `call 0x47F0B0`、`0x0047DE72` から `imul 0xc(%ebp),%esi` で 2 つの引数を掛ける）。
/// ヒープは `0x0047DEE8 call *0x48e18c` で直に取り、見張る確保の入口（malloc `0x47B151` / operator new `0x47B24E`）を通らない。その塊を返すのは見張る free `0x47B163` なので、
/// 見張りに足さないと「期間の中で取って返した塊」を「前からある塊の解放」に数える。盗むのは 2 命令（どちらも即値の push）。呼ぶ所 8 か所はどれも `0x0047_7BCB` より後（`GAME_CODE_END` の外）なので、DLL の領域へは向けずに数えるだけ。
pub const CALLOC_SITES: &[SiteDef] = &[SiteDef {
    name: CALLOC_SITE,
    addr: 0x0047_DE66,
    expected: &[0x6A, 0x10, 0x68, 0x40, 0x56, 0x49, 0x00],
    kind: Use::Detour {
        instr_lens: &[2, 5],
    },
    source: site_source!("ノート §30（th09_disasm.txt L168927〜168928）"),
}];

/// 更新の列のつなぎ（レジストリの先頭の 2 つの節）の大きさ。先頭の 0x20 B が更新の列の先頭の節（`0x0042C716 mov -0x4(%ebp),%esi`）、`+0x20` の 0x20 B が描画の列の先頭の節（`0x0042C7EB lea 0x20(%ecx),%esi`）。
/// 予測の tick でラウンドの終わりが列に節を足し得るので、コピーの範囲に足す。残りの `+0x40`〜`+0x4F`（`0x4AD008`〜`0x4AD018`）は列ではなく別の物（`REGISTRY_TAIL_OBJ`）なので、コピーするのは先頭の 0x40 B だけ。
pub const UPDATE_REGISTRY_BYTES: u32 = 0x40;
/// レジストリの後ろの 16 B（`0x4AD008`）。`mov $0x4ad008,%ecx` で this にして `0x433290` / `0x4331C0` / `0x4335E0` / `0x4333B0` を呼ぶ所が 6 か所。`0x4333B0` は `+0x08` を import `*0x48e05c` へ渡して 0 に、
/// `+0x00` を `0x433160` で、`+0x0C` を vtable の `+0x1C` で片付けて 0 にする —— ファイルの読み込みの処理と見られる。マッチの tick の中で変わる所は見つかっていないのでコピーしない（持ち物のハンドルを書き戻すと、閉じた物を指し得る）。
pub const REGISTRY_TAIL_OBJ: u32 = 0x004A_D008;
const _: () = assert!(UPDATE_REGISTRY + UPDATE_REGISTRY_BYTES == REGISTRY_TAIL_OBJ);

/// 製品の経路でコピーする .data の範囲（E1 の `ROLLBACK_DATA` の 7 行 ＋ 更新の列のつなぎ。ただし `globals_c` は fps の計測の 6 語 `FPS_TIMING_START`〜`FPS_TIMING_END` の後から）。
/// E1 の表は変えない。
pub const ROLLBACK_PLAY_DATA: &[RbData] = &ROLLBACK_PLAY_DATA_ROWS;
const ROLLBACK_PLAY_DATA_ROWS: [RbData; ROLLBACK_DATA.len() + 1] = {
    let mut rows = [RbData {
        name: "update_registry",
        start: UPDATE_REGISTRY,
        end: UPDATE_REGISTRY + UPDATE_REGISTRY_BYTES,
        source: site_source!(
            "更新の列の頭の節 0x4ACFC8・描画の列の頭の節 +0x20（0x0042C716 / 0x0042C7EB）"
        ),
    }; ROLLBACK_DATA.len() + 1];
    let mut i = 0;
    while i < ROLLBACK_DATA.len() {
        rows[i] = ROLLBACK_DATA[i];
        i += 1;
    }
    rows[GLOBALS_C_ROW] = RbData {
        name: "globals_c",
        start: FPS_TIMING_END,
        end: ROLLBACK_DATA[GLOBALS_C_ROW].end,
        source: site_source!(
            "E1 の globals_c から fps の計測の 6 語（0x4B42B8〜0x4B42CF。読み書きは 0x004316C2〜0x004317F3 だけ）を除いた残り"
        ),
    };
    rows
};
// つなぎは `rng_input` の終わりから始まり、E1 が「除いた」隙間（`rng_input` の終わり〜`globals_b` の始まりの 0x50 B）のうち、後ろの 16 B（`REGISTRY_TAIL_OBJ`）だけを残す。
const _: () = assert!(
    ROLLBACK_DATA[2].end == UPDATE_REGISTRY && ROLLBACK_DATA[3].start == REGISTRY_TAIL_OBJ + 0x10
);

// ---- rollback のマッチの fps の表示と陣の背景（中の名前は Stage）の VM ----
// 値の転記ミスは `rollback_play` の検査 `m8_7_values_appear_in_the_note` が見る。

/// fps の計測の 6 語（`[FPS_TIMING_START, FPS_TIMING_END)`）。fps の計測と表示 `0x4316B0`（描画の優先度 31 のノード `0x431B30` から。読み込みのスレッドの `0x41B566` からも呼ぶ）だけが読み書きする
/// （`0x4b42b8`〜`0x4b42cf` を即値で引く 22 行は全部 `0x004316C2`〜`0x004317F3` の中）。中身は描画の回数 `0x4B42CC`・基準の時刻 `0x4B42C4`（timeGetTime）・`0x4B42C8` の初回の印・
/// `0x4B42BC`〜`0x4B42C0`（`[0x4B3844]` ≠ 0 のときのもう 1 つの時計の経路。import `*0x48e098`）・`0x4B42B8`（その経路で計算し直した回数）。
/// E1 ではコピーと書き戻しをし、比べからだけ外していた（`ROLLBACK_COMPARE_SKIPS` の `fps_timing`）。製品の経路では書き戻しからも外す: 書き戻すと描画の回数がコピーの時の値に戻り、基準の時刻も古いまま残るので、
/// `0x43171D cmp $0x1f4`（500 ms）が毎フレーム成り立って fps を少ない回数で計算し直し、右下の表示と、記録の処理 `0x420348` がリプレイへコピーする丸めた値 `FPS_SHOWN` が狂う。
/// 更新（走査）の側は 6 語を読まないので、書き戻さなくてもマッチの決定性は変わらない（更新が読む `FPS_SHOWN` はコピーの外 —— ウィンドウと Supervisor の範囲 `0x4B30B0`〜`0x4B38B0`）。
pub const FPS_TIMING_START: u32 = 0x004B_42B8;
pub const FPS_TIMING_END: u32 = 0x004B_42D0;
/// 描画の回数（u32。`0x4316C2 mov 0x4b42cc,%ecx` / `0x4316D5 mov %edx,0x4b42cc` / `0x4317F3 movl $0x0,0x4b42cc`）。書き戻しの
/// 確かめ（`PlayStats::fps_words_restored`）が読む。
pub const FPS_DRAWS: u32 = 0x004B_42CC;
/// fps の表示の丸めた値（i16。`0x4318C3 mov %ax,0x4b36c8`。記録の処理 `0x00420348 mov 0x4b36c8,%al` がリプレイへコピーする）。
pub const FPS_SHOWN: u32 = 0x004B_36C8;
/// 基準の時刻（timeGetTime の値。`0x4316FA` / `0x43170D` / `0x43173F mov %eax,0x4b42c4`）。偽のゲームの fps の真似だけが使う。
#[cfg(test)]
pub const FPS_BASE_MS: u32 = 0x004B_42C4;
/// E1 の `ROLLBACK_DATA` の `globals_c` の行の番号（製品の経路では始まりを `FPS_TIMING_END` にする）。
const GLOBALS_C_ROW: usize = 5;
const _: () = assert!(
    ROLLBACK_DATA[GLOBALS_C_ROW].start == FPS_TIMING_START
        && ROLLBACK_DATA[GLOBALS_C_ROW].end > FPS_TIMING_END
        && FPS_TIMING_START <= FPS_DRAWS
        && FPS_DRAWS + 4 <= FPS_TIMING_END
);

/// 陣の背景（中の名前は Stage）の ANM の VM の配列 `bgscroll` のポインタ（Stage `+0x24`）と本数（Stage `+0x824`。i32）。読み込み `0x402570` が std の先頭の i16（`0x4025B2 movswl 0x2(%eax)`）から本数を決め、
/// `0x402611`〜`0x402633`（`imul $0x2a4` → `push $0x48e2cc`（"bgscroll"）→ 確保の包み `call 0x401340` → `mov %eax,0x24(%edi)`）で取る。読み込みのスレッドの確保なので CRT の塊で、塊のコピーにも DLL の領域にも入らない。
/// 更新 `Stage::on_tick 0x402350` が毎 tick 物体ごとの quad の VM を `0x401241 call 0x436f30`（run_vm）で 1 tick 進めるので、コピーしないと書き戻しの後の進め直しの tick の分だけ背景の動きが先へ進む。
pub const STAGE_BG_VMS: u32 = 0x24;
pub const STAGE_BG_VM_COUNT: u32 = 0x824;
/// コピーする VM の本数の上限（陣ごと）。th09.dat の std 15 本の本数は 6〜54（world00.std が最大。先頭の i16 と、読み込みと同じ歩き方で数え直した数が 15 本とも一致）。
/// 約 2.4 倍の 128 本（86,528 B）を置き場に取る。
pub const BG_VM_MAX: u32 = 128;
/// VM の時計（`zAnmVm.current_time_in_script` の `current`。zTimer `+0x38` の `+0x08`）。run_vm の先頭 `0x436F86 lea 0x38(%edi),%ebx` → `0x436F8B call 0x435F00`
/// （`mov 0x8(%ecx),%eax`）で読み、尻 `0x43939E lea 0x38(%edi),%ecx` → `0x4393A1 call 0x401510`（→ `0x4014B0`。前の値を `+0x00` へコピー、Supervisor の速さで進める）で 1 tick 進める。
pub const VM_CLOCK: u32 = 0x40;
/// VM の今の命令（`zAnmVm.current_instr`）。0 なら run_vm は何もせず 1 を返す（`0x436F3D`〜`0x436F56`）—— 終わった VM。
pub const VM_CUR_INSTR: u32 = 0x220;

/// 製品の経路でコピーする VM の配列（陣ごとに 1 行。`Tables::th09_play` が使う。E1 の表には入れない）。
pub const ROLLBACK_VM_ARRAYS: &[RbVmArray] = &[
    RbVmArray {
        name: "p1_bgscroll",
        owner: STAGE_BLOCKS[0],
        ptr_off: STAGE_BG_VMS,
        count_off: STAGE_BG_VM_COUNT,
        elem: VM_BYTES,
        max: BG_VM_MAX,
        source: site_source!(
            "0x402611 mov 0x824(%edi) / 0x402617 imul $0x2a4 / 0x402628 call 0x401340 / 0x402633 mov %eax,0x24(%edi)"
        ),
    },
    RbVmArray {
        name: "p2_bgscroll",
        owner: STAGE_BLOCKS[1],
        ptr_off: STAGE_BG_VMS,
        count_off: STAGE_BG_VM_COUNT,
        elem: VM_BYTES,
        max: BG_VM_MAX,
        source: site_source!("p1_bgscroll と同じ（Stage は陣ごとに読み込む）"),
    },
];

/// th09.exe の .data（VA `0x004A0000`〜`0x004E41FC`）。起動から終わりまでコミット済みで読み書きできる頁なので、更新の列の節がここにあれば直に読む
/// （列の並びの見張りを 1 tick ごとに回すため。外はシステムの関数で読む）。
pub const IMAGE_DATA_START: u32 = 0x004A_0000;
pub const IMAGE_DATA_END: u32 = 0x004E_41FC;

// ---- 描画が消す欄（カットインの赤い点滅の直し）----
// 「更新が立てて描画が使ってから消す」欄は、進め直しの途中のコピー（描画を通らない）に立った値のまま残り、書き戻しのたびに戻る。コピーを取るたびに、
// コピーの中のその欄だけを描画の後の値へ置き換える（`higanr_core::render_reset`。ゲームのメモリには書かない）。

/// Stage の背景の色（4 B。`+0x642C`〜`+0x642E` が RGB、`+0x642F` が α）。書くのは更新 `Stage::on_tick 0x402350`（優先度 8 + 陣）の `0x402378 movl $0x80303040`（模様 2 = 暗）・`0x402384 movl $0x80f00000`（模様 1 = 赤）・
/// `0x4023A6 movl $0x40303030`（`[+0x10]+0x34` の bit 0 のとき。模様によらない）と、描画 `0x403680` の `0x4036ED`〜`0x403700`（α を 0、RGB を 0x80 に）だけ。
/// 読むのは描画 `0x4033E0`（`0x403459` / `0x403463`）と `0x403680`（`0x4036CF` / `0x4036D9`）だけで、α が 0 でなければ AnmManager の色 `0x401620` に渡す。
pub const STAGE_TINT: u32 = 0x642C;
/// Stage の色の模様（u32。0 = なし・1 = 赤・2 = 暗）。更新 `0x402353` が読み、0 なら色を書かない（`0x40235B je`）。0 でなければ残り `+0x20` を 1 減らし、
/// 尽きたら模様と残りを 0 に（`0x40236A` / `0x40236D`）。立てるのはスペルカードの始まり `0x403E50` の `0x404155 movl $0x1`（片方の陣）/ `0x40416E movl $0x2`
/// （もう片方）で、残りは 0x1E tick（`0x404161`）。
pub const STAGE_TINT_MODE: u32 = 0x1C;
// 模様の欄（と残り `+0x20`）は色の欄と重ならない（書き戻しの直後の確かめが色の欄を模様と読まない）。
const _: () = assert!(STAGE_TINT_MODE + 8 <= STAGE_TINT);
/// 描画 `0x403680` が Stage の色を戻す命令（`0x4036EB mov $0x80,%al` / `0x4036ED movb $0x0,0x642f(%edi)` / `0x4036F4`・`0x4036FA`・
/// `0x403700 mov %al,0x642e / 0x642d / 0x642c(%edi)`）。照合だけで書かない。
pub const STAGE_TINT_RESET_AT: u32 = 0x0040_36EB;
pub const STAGE_TINT_RESET_CODE: &[u8] = &[
    0xB0, 0x80, // mov $0x80,%al
    0xC6, 0x87, 0x2F, 0x64, 0x00, 0x00, 0x00, // movb $0x0,0x642f(%edi)
    0x88, 0x87, 0x2E, 0x64, 0x00, 0x00, // mov %al,0x642e(%edi)
    0x88, 0x87, 0x2D, 0x64, 0x00, 0x00, // mov %al,0x642d(%edi)
    0x88, 0x87, 0x2C, 0x64, 0x00, 0x00, // mov %al,0x642c(%edi)
];
/// `STAGE_TINT_RESET_CODE` の命令の長さ（検査が 1 命令ずつ読み、ノートからの転記と突き合わせる）。
#[cfg(test)]
pub const STAGE_TINT_RESET_LENS: &[usize] = &[2, 7, 6, 6, 6];
/// 描画の後の Stage の色（`+0x642C`〜`+0x642F` の順）。
pub const STAGE_TINT_AFTER: [u8; 4] = [0x80, 0x80, 0x80, 0x00];
/// Stage の塊の名前（`ROLLBACK_BLOCKS` の `name`。1P・2P の順）。書き戻しの直後の確かめ（`PlayStats::tint_stale`）もこの 2 つを見る。
pub const STAGE_BLOCKS: [&str; 2] = ["p1_stage", "p2_stage"];

/// 描画が消す欄の表（コピーを取るたびにコピーの中だけで置き換える）。**入れる条件**: 更新が立て、描画だけが使って消す欄（描画が世界の欄へ流さない）。
/// 入れないもの: Stage `+0xCD4`（描画 `0x4033E0` の中の読み `0x403425` と 0 書き `0x40344F` だけで、更新の側の書き手は見つからなかった）・エフェクト `+0xC9`
/// （描画 `0x40DC70` が 1 のときだけ頂点を作り直して 0 にする「作り直しの印」だが、同じ種別の更新 `0x40D470` が毎 tick 無条件に 1 を書く（`0x40D4C0`）ので、書き戻しの後は描画の前に必ず 1 になり、
/// コピーの中の値によらず描画は同じ）。
pub const RENDER_RESETS: &[RbRenderReset] = &[
    RbRenderReset {
        block: STAGE_BLOCKS[0],
        offset: STAGE_TINT,
        after: &STAGE_TINT_AFTER,
        code_at: STAGE_TINT_RESET_AT,
        code: STAGE_TINT_RESET_CODE,
        source: site_source!(
            "更新 0x402378 / 0x402384 / 0x4023A6 が立て、描画 0x403680 の 0x4036ED〜0x403700 が α 0・RGB 0x80 に戻す"
        ),
    },
    RbRenderReset {
        block: STAGE_BLOCKS[1],
        offset: STAGE_TINT,
        after: &STAGE_TINT_AFTER,
        code_at: STAGE_TINT_RESET_AT,
        code: STAGE_TINT_RESET_CODE,
        source: site_source!("p1_stage と同じ（描画 0x403680 は陣ごとに登録される）"),
    },
];

// ---- リプレイの自動保存（マッチの終わりのメニューの間に、入力の地点から保存関数を直に呼ぶ）----
// 地点の表（`SITES`）には入れない（自動保存を入れない実行の設置の条件を変えないため）。自動保存を入れるとき（`replay_save_win::arm`）と、呼ぶ直前（`RealSaveIo::signature_ok`）に照合する。
// 検査 `addresses_appear_in_the_note`。
/// 保存関数 `ReplayManager::save`（`__fastcall(this = ecx, path = edx, name = [esp+4])`、`0x00421133 ret $0x4`）。
pub const REPLAY_SAVE_FN: u32 = 0x0042_0C60;
/// その先頭の 12 バイト（`push ebp; mov ebp,esp; sub esp,0x618; push edi; mov edi,ecx`）。
pub const REPLAY_SAVE_SIG: [u8; 12] = [
    0x55, 0x8B, 0xEC, 0x81, 0xEC, 0x18, 0x06, 0x00, 0x00, 0x57, 0x8B, 0xF9,
];
/// 保存関数の中の ebp は、呼ぶ `call` の時点の esp より 8 小さい（戻りアドレスと `0x00420C60 push ebp`）。
pub const REPLAY_SAVE_FRAME_SHIFT: u32 = 8;
// マッチの終わりのメニューが出ている間 1 の 1 バイトの欄 `MATCH_END_MENU`（Match だけ。Story / Extra は `0x4A7ECD`）と、席ごとの勝ち数 `P1_WINS` / `P2_WINS` は、
// 上の確保・解放の段階に 1 つだけ置く。
/// exe のフォルダ（`char[0x104]`。起動時に `0x0042E3C9` の `GetModuleFileNameA` で埋まり、最後の区切りを `/` にする）。
pub const EXE_DIR: u32 = 0x004A_CD08;
pub const EXE_DIR_LEN: u32 = 0x104;
/// `GAME_MODE` の Match の値（0 Story / 1 Extra / 2 Match）。
pub const GAME_MODE_MATCH: u32 = 2;
/// 勝ちに要るラウンドの数と席ごとの勝ち数（u32）。ラウンドの終わり `0x00415D8A mov 0x4a7e98(,%edi,4),%edx` →
/// `0x00415D92` で 1 増やして書き戻し → `0x00415D99 mov 0x4a7e94,%ecx` と比べる。
pub const ROUNDS_REQUIRED: u32 = 0x004A_7E94;
/// 作業領域の見積もり: 初めの値を置く `mov $imm32,%ecx`（`B9 EC 01 01 00`）の位置と、その値。
pub const SAVE_EST_BASE_SITE: u32 = 0x0042_0C8F;
pub const SAVE_EST_BASE: u32 = 0x0001_01EC;
/// 見積もりのループが面ごとの印を読むローカルの位置（`0x00420CA0 mov -0x1f8(%ebp,%esi,4),%eax`。ebp から引く値）。
pub const SAVE_EST_FLAGS_EBP: u32 = 0x1F8;
/// 面の数（`0x00420CC5 cmp $0xa,%esi`）。
pub const SAVE_EST_STAGES: usize = 10;
/// 印が 0 でない面ごとの足し分（`0x00420CAB add $0x60,%ecx`）と、塊ごとの足し分（`0x00420CB7 add $0x54d9,%ecx`）。
pub const SAVE_EST_PER_STAGE: u32 = 0x60;
pub const SAVE_EST_PER_CHUNK: u32 = 0x54D9;
/// ReplayManager の中: ヘッダへのポインタ（`0x00420CDA mov 0x8(%edi),%esi`）と、そのヘッダの長さ（`0x00420CDD mov $0x7b,%ecx`
/// の 0x7B 語）。ヘッダの中の面ごとの印（ローカルのヘッダ `-0x218` から見た `-0x1F8` の位置 = `+0x20`）。
pub const REPLAY_HEADER_PTR: u32 = 0x08;
pub const REPLAY_HEADER_LEN: u32 = 0x1EC;
pub const REPLAY_HEADER_STAGE_FLAGS: u32 = 0x20;
/// 見出しの中のオプションの塊の位置（読み込みスレッドが `th09.cfg` 由来のオプションを 1 回だけ写す。0xCC バイトで、並びは `OPTIONS_BLOCK` と同じ）。
/// 値は `higanr_core::replay_rewrite::OPTIONS_OFFSET`、写す箇所は `0x00421266`〜`0x00421271`。
pub const REPLAY_HEADER_OPTIONS: u32 = 0xDC;
/// 見出しの中のチャージ型（1P が `+0x190`、2P が `+0x191`）。ここが生きた試合の値と違うと、素の花映塚の再生がずれる。
pub const REPLAY_HEADER_CHARGE: u32 = REPLAY_HEADER_OPTIONS + OPT_CHARGE;
/// 面ごとの塊の列の先頭（`0x00420C8C lea 0x1c(%edi),%edx`）、その刻み（`0x00420CC2 add $0x18,%edx`）、塊の次
/// （`0x00420CB4 mov 0x14(%eax),%eax`）。
pub const REPLAY_STAGE_NODES: u32 = 0x1C;
pub const REPLAY_STAGE_STRIDE: u32 = 0x18;
pub const REPLAY_NODE_NEXT: u32 = 0x14;

// ---- ネットワークの対戦の「リプレイを保存して終了」で保存の画面を飛ばす ----
// 地点の表（`SITES`）には入れない。書かずに照合だけする行で、立てる tick に `script_win` が読んで合わなければ印を立てない（いつもの保存の画面になる）。検査 `save_skip_values_appear_in_the_note`。
/// 画面 15（リプレイの保存）の副状態 0 の最初のフレームの分かれ道: `0x00429B5E testb $0x20,0x4a7ec5`（7 バイト）→ `0x00429B65 je 0x429b7a`（2 バイト）。印が立っていれば `0x00429B6E` で set_screen(1)（タイトル）へ移り、何も保存しない。
pub const SAVE_SKIP_SITE: SiteDef = SiteDef {
    name: "save_skip_branch",
    addr: 0x0042_9B5E,
    expected: &[0xF6, 0x05, 0xC5, 0x7E, 0x4A, 0x00, 0x20, 0x74, 0x13],
    kind: Use::VerifyOnly {
        instr_lens: Some(&[7, 2]),
    },
    source: site_source!("ノート th09_replay_save_M5b.md §11-1（th09_disasm.txt L52661〜L52662）"),
};
/// 分岐が見る印: グローバル状態語 `GLOBAL_STATE` の bit13（上の `testb $0x20` は 2 バイト目 `0x4A7EC5` の bit5）。ゲームが立てるのは Story のコンティニュー
/// （`0x00435268 orl $0x2000,0x4a7ec4`）だけ、読むのは上の分かれ道だけ。
pub const CONTINUED_BIT: u32 = 0x2000;

#[cfg(test)]
mod save_skip_tests {
    use super::*;
    use crate::sites::hex_bytes;

    #[test]
    fn save_skip_values_appear_in_the_note() {
        const NOTE: &str = include_str!("../../../analysis/notes/th09_replay_save_M5b.md");
        let s = SAVE_SKIP_SITE;
        for w in [
            format!("`0x{:08X}`（9 バイト `{}`", s.addr, hex_bytes(s.expected)),
            format!("`0x{:08X} testb $0x20,0x4a7ec5`", s.addr),
            format!(
                "`0x{:08X} orl $0x{CONTINUED_BIT:x},0x4a7ec4`",
                0x0043_5268u32
            ),
        ] {
            assert!(NOTE.contains(&w), "ノートに {w} が無い");
        }
        let lens: usize = match s.kind {
            Use::VerifyOnly {
                instr_lens: Some(l),
            } => l.iter().map(|&n| usize::from(n)).sum(),
            other => panic!("照合だけの行でない: {other:?}"),
        };
        assert_eq!(lens, s.expected.len());
        let at = u32::from_le_bytes([s.expected[2], s.expected[3], s.expected[4], s.expected[5]]);
        let bit = u32::from(s.expected[6]) << (8 * (at - GLOBAL_STATE));
        assert_eq!(&s.expected[..2], &[0xF6, 0x05]);
        assert!((GLOBAL_STATE..GLOBAL_STATE + 4).contains(&at));
        assert_eq!(bit, CONTINUED_BIT);
    }
}

#[cfg(test)]
mod tests {
    //! 表そのものの検査。花映塚は使わない。期待バイト列の転記ミスは、表と同じ出所から作った値ではなく、ノートの本文（`include_str!`）と突き合わせて見る。
    use super::*;
    #[cfg(not(feature = "dist"))]
    use crate::sites::hex_bytes;
    use crate::sites::{TableError, check_table};
    use higanr_core::patch::{DisjointError, SiteError, check_site};

    const NOTE: &str = include_str!("../../../analysis/notes/th09_hook_sites_M1a.md");

    fn find(name: &str) -> SiteDef {
        let hits: Vec<&SiteDef> = SITES.iter().filter(|d| d.name == name).collect();
        assert_eq!(hits.len(), 1, "{name} は表にちょうど 1 行あるはず");
        *hits[0]
    }

    fn with_replaced(name: &str, new: SiteDef) -> Vec<SiteDef> {
        let _ = find(name);
        SITES
            .iter()
            .map(|d| if d.name == name { new } else { *d })
            .collect()
    }

    // ---- 表そのもの ----

    #[test]
    fn the_table_passes_check_table() {
        assert_eq!(check_table(SITES, FORBIDDEN), Ok(()));
    }

    #[test]
    fn every_detour_site_passes_check_site_with_the_forbidden_ranges() {
        let mut n = 0;
        for def in SITES {
            if let Use::Detour { instr_lens } = def.kind {
                let site = higanr_core::patch::Site {
                    name: def.name,
                    addr: def.addr,
                    expected: def.expected,
                    instr_lens,
                };
                assert_eq!(check_site(&site, FORBIDDEN), Ok(()), "{}", def.name);
                n += 1;
            }
        }
        assert_eq!(n, 5);
    }

    #[test]
    fn instruction_lengths_sum_to_the_expected_length() {
        let mut n = 0;
        for def in SITES {
            let lens: Option<&[u8]> = match def.kind {
                Use::Detour { instr_lens } => Some(instr_lens),
                Use::CallRel32 { .. } => Some(&[5]),
                Use::VerifyOnly { instr_lens } => instr_lens,
            };
            if let Some(lens) = lens {
                let sum: usize = lens.iter().map(|&l| l as usize).sum();
                assert_eq!(sum, def.expected.len(), "{}", def.name);
                n += 1;
            }
        }
        assert_eq!(n, SITES.len() - 4);
    }

    #[test]
    fn site_names_are_unique() {
        for d in SITES {
            let _ = find(d.name);
        }
    }

    /// 期待バイト列かアドレスの転記ミス（1 バイトでも、アドレスの ±1 でも）を見つける。各行について、`source` に書いた節をノートから切り出し、① 期待バイト列がその節にあるか、
    /// ② アドレスが節の見出しの行と合うか（本文のどこかにあるかで見ると、隣のアドレスで ±1 のずれを見逃す）、③ 見出しにアドレスが無い節は、バイト列を含む表の行にこの行のアドレスがあるか、を見る。
    /// call 先の差し替えの行は、見出しがオペコードの位置か rel32 の位置のどちらかで書かれている。rel32 の位置なら、見出しに addr+1 があり、それがノートで rel32 の位置と名指しされ、ノートに addr もある、を求める。
    /// 配るバージョンのビルドでは走らせない（`source` の欄は配るバージョンでは空）。
    #[cfg(not(feature = "dist"))]
    #[test]
    fn every_site_appears_in_the_note_with_its_address() {
        let sec0 = note_top_section("## 0. ").expect("ノートに §0 が無い");
        let sec0_item2 = sec0
            .lines()
            .find(|l| l.starts_with("2. "))
            .expect("ノート §0 に「2. 」の項が無い");
        // ノートの「命令の先頭ではなく rel32 の位置」と述べた先頭の文の主語の部分。項の全体にはオペコードの位置のアドレスも出てくるので、全体では引かない。
        let rel32_named = sec0_item2
            .split("は「命令の先頭」ではなく")
            .next()
            .filter(|head| head.len() < sec0_item2.len())
            .expect("ノート §0 の 2 に「は「命令の先頭」ではなく」の文が無い");
        for def in SITES {
            let sec_id = def
                .source
                .strip_prefix("ノート §")
                .and_then(|s| s.split('（').next())
                .unwrap_or_else(|| panic!("{}: source の形が「ノート §x-y（…）」でない", def.name));
            let section = note_section(&format!("### {sec_id} "))
                .unwrap_or_else(|| panic!("{}: ノートに節 {sec_id} が無い", def.name));
            let heading = section.lines().next().unwrap_or("");
            let addr = format!("0x{:08X}", def.addr);
            let hex = hex_bytes(def.expected);
            assert!(
                section.lines().any(|l| l.contains(&hex)),
                "{}: 節 {sec_id} に {hex} が無い",
                def.name
            );

            if sec_id == "1-11" {
                let line = section
                    .lines()
                    .find(|l| l.starts_with('|') && l.contains(&hex))
                    .unwrap_or_else(|| panic!("{}: 節 1-11 の表に {hex} の行が無い", def.name));
                assert!(
                    line.contains(&addr),
                    "{}: {hex} の表の行が {addr} でない",
                    def.name
                );
                continue;
            }

            match def.kind {
                Use::CallRel32 { .. } => {
                    let rel32 = format!("0x{:08X}", def.addr.wrapping_add(1));
                    let at_opcode = heading.contains(&addr) && !rel32_named.contains(&addr);
                    let at_rel32 = heading.contains(&rel32)
                        && rel32_named.contains(&rel32)
                        && sec0_item2.contains(&addr);
                    assert!(
                        at_opcode || at_rel32,
                        "{}: 節 {sec_id} の見出しが {addr}（オペコードの位置）とも {rel32}（rel32 の位置。§0 の 2 に {addr}）とも合わない",
                        def.name
                    );
                }
                Use::Detour { .. } | Use::VerifyOnly { .. } => {
                    assert!(
                        heading.contains(&addr),
                        "{}: 節 {sec_id} の見出しに {addr} が無い",
                        def.name
                    );
                }
            }
        }
    }

    #[test]
    fn record_offsets_appear_in_the_design() {
        const DESIGN: &str = include_str!("../../../docs/Design_Architecture.md");
        let head = "※2026-09-25（M1b の tick ごとの記録";
        let start = DESIGN.find(head).expect("設計に tick ごとの記録の項が無い");
        let rest = &DESIGN[start..];
        let item = &rest[..rest.find("\n- ").unwrap_or(rest.len())];
        let mut want = vec![
            format!("0x{WINDOW_ADDR:08X}"),
            format!("0x{:X}", WINDOW_ADDR + WINDOW_CLOCK_DIVISOR),
        ];
        for off in [
            WINDOW_CLOCK_DIVISOR,
            WINDOW_NOW,
            WINDOW_DEADLINE,
            SUP_FRAME,
            SUP_SCENE_CUR,
            SUP_SCENE_REQ,
        ] {
            want.push(format!("+0x{off:X}"));
        }
        for w in want {
            assert!(item.contains(&w), "設計の tick ごとの記録の項に {w} が無い");
        }
    }

    #[test]
    fn observation_addresses_appear_in_the_design() {
        const DESIGN: &str = include_str!("../../../docs/Design_Architecture.md");
        const POINTERS: &str =
            include_str!("../../../docs/investigation/TH09_Knowledge_Pointers.md");
        let head = "※2026-09-26（M1b の観測のバージョン";
        let start = DESIGN.find(head).expect("設計に観測の版の項が無い");
        let rest = &DESIGN[start..];
        let item = &rest[..rest.find("\n- ").unwrap_or(rest.len())];
        let mut addrs = vec![
            SEATS_RANGE,
            SEATS_RANGE + SEATS_LEN,
            MATCH_RANGE,
            MATCH_RANGE + MATCH_LEN,
            RNG_ADDR,
            RNG_ADDR + RNG_LEN,
            P1_PLAYER_PTR,
            P2_PLAYER_PTR,
            P1_CHAR,
            P2_CHAR,
            P1_CONTROL,
            P2_CONTROL,
            BACKGROUND,
            SELECT_KIND,
            GAME_MODE,
            DIFFICULTY,
            LOADING,
            MAP,
            PAUSE,
            LOAD_DONE,
            MATCH_REGISTERED,
            MAIN_MENU_PTR,
        ];
        addrs.extend(INPUT_MANAGERS);
        for a in addrs {
            let w = format!("0x{a:X}");
            assert!(item.contains(&w), "設計の観測の版の項に {w} が無い");
        }
        for len in [SEATS_LEN, MATCH_LEN, RNG_LEN] {
            assert!(
                item.contains(&format!("| {len} |")),
                "設計の表に長さ {len} が無い"
            );
        }
        assert!(item.contains(&format!("0x{INPUT_COPY_LEN:X} ずつ")));
        for (name, off) in [
            ("screen_id", MENU_SCREEN_ID),
            ("substate", MENU_SUBSTATE),
            ("cursor", MENU_CURSOR),
            ("prev_screen", MENU_PREV_SCREEN),
            ("transitioning", MENU_TRANSITIONING),
            ("anim", MENU_ANIM),
        ] {
            let w = format!("{name} +0x{off:X}");
            let w2 = format!("`{name} +0x{off:X}`");
            let w3 = format!("{name} +0x{off:02X}");
            assert!(
                item.contains(&w) || item.contains(&w2) || item.contains(&w3),
                "設計の項に {w} が無い"
            );
        }
        assert!(item.contains(&format!("[0x{P1_PLAYER_PTR:X}]+0x{PLAYER_LIFE:X}")));
        assert!(item.contains(&format!("[0x{P2_PLAYER_PTR:X}]+0x{PLAYER_LIFE:X}")));
        assert!(POINTERS.contains(&format!("`0x{RNG_COUNT:08X}`")));
        let im_row = POINTERS
            .lines()
            .find(|l| l.contains("InputManager 3 本"))
            .expect("索引に InputManager の行が無い");
        for a in INPUT_MANAGERS {
            assert!(
                im_row.contains(&format!("`0x{a:08X}`")),
                "索引の行に 0x{a:08X}"
            );
        }
        assert!(im_row.contains(&format!(
            "0x{:X} 刻み",
            INPUT_MANAGERS[1] - INPUT_MANAGERS[0]
        )));
        assert_eq!(
            INPUT_MANAGERS[2] - INPUT_MANAGERS[1],
            INPUT_MANAGERS[1] - INPUT_MANAGERS[0]
        );
        for off in [
            IM_CUR, IM_PREV, IM_REPEAT, IM_DOWN, IM_UP, IM_CHARGE, IM_GAME,
        ] {
            assert!(
                im_row.contains(&format!("`+0x{off:02X}`")),
                "索引の InputManager の行に +0x{off:02X} が無い"
            );
        }
        // CSV の列にするグローバルは、コピーする範囲のどれかの中にある（範囲の外なら列が空になる）。
        for a in [
            P1_CHAR,
            P2_CHAR,
            P1_CONTROL,
            P2_CONTROL,
            BACKGROUND,
            SELECT_KIND,
            GAME_MODE,
            DIFFICULTY,
            LOADING,
            MAP,
            PAUSE,
        ] {
            assert!(
                (SEATS_RANGE..SEATS_RANGE + SEATS_LEN - 3).contains(&a),
                "0x{a:08X}"
            );
        }
        for a in [LOAD_DONE, MATCH_REGISTERED] {
            assert!(
                (MATCH_RANGE..MATCH_RANGE + MATCH_LEN - 3).contains(&a),
                "0x{a:08X}"
            );
        }
        assert!((RNG_ADDR..RNG_ADDR + RNG_LEN - 3).contains(&RNG_COUNT));
    }

    #[test]
    fn risk17_gui_pointer_appears_in_the_note() {
        let sec9 = note_top_section("## 9. ").expect("ノートに §9 が無い");
        assert!(sec9.contains(&format!("0x404980([0x{GUI_PTR:X}])")));
    }

    #[test]
    fn shot_addresses_appear_in_the_note() {
        const D3D: &str = include_str!("../../../analysis/notes/th09_title_and_d3d8.md");
        let sec = |head: &str| -> &'static str {
            let start = D3D
                .find(head)
                .unwrap_or_else(|| panic!("ノートに {head} が無い"));
            let rest = &D3D[start + head.len()..];
            let end = rest
                .find("\n### ")
                .into_iter()
                .chain(rest.find("\n## "))
                .min()
                .unwrap_or(rest.len());
            &rest[..end]
        };
        let s21 = sec("### 2-1 ");
        let s26 = sec("### 2-6 ");
        assert!(s21.contains(&format!("`0x{SUPERVISOR:08X} = SUPERVISOR`")));
        assert!(s21.contains(&format!(
            "`+0x{:02X} = d3d_device`",
            D3D8_DEVICE_PTR - SUPERVISOR
        )));
        assert!(s26.contains(&format!("`0x{D3D8_PRESENT_PARAMS:X}` へ写す")));
        assert!(s26.contains(&format!(
            "`[Supervisor+0x{:X}]`（= `0x{:X}`",
            D3D8_PRESENT_PARAMS + PP_FORMAT - SUPERVISOR,
            D3D8_PRESENT_PARAMS + PP_FORMAT
        )));
        for (slot, what) in [
            (D3D8_GETBACKBUFFER_SLOT, "GetBackBuffer"),
            (D3D8_SURF_LOCKRECT_SLOT, "LockRect"),
            (D3D8_SURF_UNLOCKRECT_SLOT, "UnlockRect"),
        ] {
            let w = format!("call *0x{slot:x}(%ecx)` = ");
            let line = s26
                .lines()
                .find(|l| l.contains(&w))
                .unwrap_or_else(|| panic!("§2-6 に {w} が無い"));
            assert!(line.contains(what), "§2-6 の {w} の行が {what} でない");
        }
        assert!(s26.contains(&format!(
            "面の `+0x{D3D8_SURF_GETDESC_SLOT:02X}`（GetDesc）と `+0x{D3D8_SURF_RELEASE_SLOT:02X}`（Release）"
        )));
        assert!(s26.contains("幅 `-0x44`") && s26.contains("高さ `-0x40`"));
        assert!(s26.contains("形式 `-0x3C`"));
        assert_eq!((PP_WIDTH, PP_HEIGHT, PP_FORMAT), (0, 4, 8));
    }

    #[test]
    fn script_function_rows_and_counter_offset() {
        for name in [FEED_SITE, LOCAL_INPUT_SITE] {
            let d = find(name);
            assert!(
                matches!(d.kind, Use::VerifyOnly { instr_lens: None }),
                "{name} は関数の入口の照合だけの行のはず"
            );
        }
        const A0: &str = include_str!("../../../analysis/notes/th09_input_and_wait_A0.md");
        assert!(A0.contains(&format!("`cnt[b]` は `+0x{IM_REPEAT_COUNTERS:02X} + 2*b`")));
        assert!(A0.contains(&format!(
            "書くのは `+0x00`〜`+0x{:X}` だけ",
            IM_FEED_LEN - 1
        )));
    }

    #[test]
    fn shot_word_is_the_pressed_word_of_the_composite_input_manager() {
        const A0: &str = include_str!("../../../analysis/notes/th09_input_and_wait_A0.md");
        const D3D: &str = include_str!("../../../analysis/notes/th09_title_and_d3d8.md");
        assert_eq!(SHOT_WORD, 0x004A_CF3A);
        assert_eq!(SHOT_BIT, 0x0800);
        // 二つのノートが、Present のラッパの中の判定 `[2]+0x06 & 0x0800` と、P と Home の割り当てを書いている。
        for note in [A0, D3D] {
            assert!(note.contains("0x0042DBAA"), "判定の地点がノートに無い");
            assert!(note.contains("[2]+0x06"), "判定の語がノートに無い");
        }
        assert!(A0.contains("P と Home=0x0800"));
        assert!(A0.contains("Present の包みの中"));
    }

    #[test]
    fn poll_row_matches_the_note_and_overlaps_nothing() {
        const A0: &str = include_str!("../../../analysis/notes/th09_input_and_wait_A0.md");
        assert_eq!(POLL_SITES.len(), 1, "行は 1 つだけ");
        let d = POLL_SITES[0];
        assert_eq!(d.name, POLL_SITE);
        assert!(
            SITES.iter().all(|s| s.name != POLL_SITE),
            "SITES（14 行）に入れない"
        );
        assert_eq!(SITES.len(), 14);
        assert!(matches!(d.kind, Use::VerifyOnly { instr_lens: None }));
        let hex = d
            .expected
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        let want = format!("| {} | `0x{:08X}` | `{hex}` |", d.name, d.addr);
        assert!(
            A0.lines().any(|l| l.starts_with(&want)),
            "ノート A0 §8 に {want} の行が無い"
        );
        assert_eq!(check_table(POLL_SITES, FORBIDDEN), Ok(()));
        let all: Vec<SiteDef> = SITES
            .iter()
            .chain(POLL_SITES)
            .chain(PORTRAIT_SITES)
            .chain(PACING_SITES)
            .chain(ALLOC_SITES)
            .chain(RESIM_SITES)
            .copied()
            .collect();
        assert_eq!(check_table(&all, FORBIDDEN), Ok(()));
        let r = d.range();
        for other in SITES {
            assert!(!r.overlaps(&other.range()), "{} と重なる", other.name);
        }
    }

    fn note_top_section(heading: &str) -> Option<&'static str> {
        let start = NOTE.find(heading)?;
        let rest = &NOTE[start + heading.len()..];
        let end = rest.find("\n## ").unwrap_or(rest.len());
        Some(&rest[..end])
    }

    #[test]
    fn call_targets_appear_in_the_note() {
        let mut n = 0;
        for def in SITES {
            if let Use::CallRel32 { original_target } = def.kind {
                let lower = NOTE.to_lowercase();
                let short = format!("call 0x{original_target:x}");
                let long = format!("call 0x{original_target:08x}");
                assert!(
                    lower.contains(&short) || lower.contains(&long),
                    "{}: ノートに {short} も {long} も無い",
                    def.name
                );
                n += 1;
            }
        }
        assert_eq!(n, 3);
    }

    #[test]
    fn forbidden_ranges_appear_in_the_note() {
        let sec21 = note_section("### 2-1 ").expect("§2-1");
        let sec22 = note_section("### 2-2 ").expect("§2-2");
        let sec23 = note_section("### 2-3 ").expect("§2-3");
        let vpatch_starts = [0x0042_DB59u32, 0x0042_F7F7, 0x0043_16C2, 0x0047_D5DF];
        let skips = [(0x0042_DB60u32, 0x0042_DB99u32)];
        let tick = (0x0042_C7B5u32, 0x0042_C7D3u32);
        assert_eq!(FORBIDDEN.len(), vpatch_starts.len() + skips.len() + 1);
        for s in vpatch_starts {
            let r = FORBIDDEN
                .iter()
                .find(|r| r.start == s)
                .unwrap_or_else(|| panic!("0x{s:08X} の禁止の範囲が無い"));
            let line = sec21
                .lines()
                .find(|l| l.starts_with(&format!("| 0x{s:08X} |")))
                .unwrap_or_else(|| panic!("§2-1 に 0x{s:08X} の行が無い"));
            let col = line.split('|').nth(4).expect("§2-1 の表の列").trim();
            let n: u32 = col
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|d| d.parse().ok())
                .unwrap_or_else(|| panic!("0x{s:08X} のバイト数が読めない: {col}"));
            assert_eq!(r.end - r.start, n, "0x{s:08X} の長さ");
        }
        for (a, b) in skips {
            assert!(
                sec22.contains(&format!("0x{a:08X}〜0x{b:08X}")),
                "§2-2 に 0x{a:08X}〜0x{b:08X}"
            );
            assert!(FORBIDDEN.contains(&AddrRange {
                start: a,
                end: b + 1
            }));
        }
        assert!(sec23.contains(&format!("0x{:08X} は ret", tick.0)));
        assert!(sec23.contains(&format!("0x{:08X}", tick.1)));
        assert!(FORBIDDEN.contains(&AddrRange {
            start: tick.0,
            end: tick.1 + 1
        }));
    }

    fn note_section(heading: &str) -> Option<&'static str> {
        let start = NOTE.find(heading)?;
        let rest = &NOTE[start + heading.len()..];
        let end = rest
            .find("\n### ")
            .into_iter()
            .chain(rest.find("\n## "))
            .min()
            .unwrap_or(rest.len());
        Some(&rest[..end])
    }

    // ---- 毒標本: 表の 1 行を壊したコピーが断られる ----

    #[test]
    fn poison_relative_branch_in_a_detour_row_is_rejected() {
        let call = find("input_wait_call");
        let bad = SiteDef {
            kind: Use::Detour { instr_lens: &[5] },
            ..call
        };
        let table = with_replaced("input_wait_call", bad);
        assert_eq!(
            check_table(&table, FORBIDDEN),
            Err(TableError::Detour {
                name: "input_wait_call",
                err: SiteError::RelativeInstruction {
                    instr_index: 0,
                    addr: 0x0043_112D
                }
            })
        );
    }

    #[test]
    fn poison_detour_row_overlapping_vpatch_is_rejected() {
        let draw = find("draw_before_endscene");
        let bad = SiteDef {
            addr: 0x0042_DB5C,
            ..draw
        };
        let table = with_replaced("draw_before_endscene", bad);
        assert_eq!(
            check_table(&table, FORBIDDEN),
            Err(TableError::Detour {
                name: "draw_before_endscene",
                err: SiteError::Forbidden {
                    range: AddrRange {
                        start: 0x0042_DB59,
                        end: 0x0042_DB60
                    }
                }
            })
        );
    }

    #[test]
    fn poison_verify_only_row_overlapping_the_tick_table_is_rejected() {
        let exit = find("render_only_exit");
        let bad = SiteDef {
            addr: 0x0042_C7B0,
            ..exit
        };
        let table = with_replaced("render_only_exit", bad);
        assert_eq!(
            check_table(&table, FORBIDDEN),
            Err(TableError::Forbidden {
                name: "render_only_exit",
                range: AddrRange {
                    start: 0x0042_C7B5,
                    end: 0x0042_C7D4
                }
            })
        );
    }

    #[test]
    fn poison_two_rows_overlapping_are_rejected() {
        let up = find("title_up_at_top");
        let bad = SiteDef {
            addr: 0x0042_45D3,
            ..up
        };
        let table = with_replaced("title_up_at_top", bad);
        assert_eq!(
            check_table(&table, FORBIDDEN),
            Err(TableError::Overlap(DisjointError {
                a: "title_down_at_edge",
                b: "title_up_at_top"
            }))
        );
    }

    #[test]
    fn poison_length_mismatch_is_rejected() {
        let down = find("title_down_at_edge");
        let bad = SiteDef {
            kind: Use::Detour {
                instr_lens: &[2, 1, 1],
            },
            ..down
        };
        assert_eq!(
            check_table(&with_replaced("title_down_at_edge", bad), FORBIDDEN),
            Err(TableError::Detour {
                name: "title_down_at_edge",
                err: SiteError::LenSumMismatch {
                    sum: 4,
                    expected_len: 5
                }
            })
        );

        let cur = find("poll_cur_read");
        let bad = SiteDef {
            kind: Use::VerifyOnly {
                instr_lens: Some(&[2, 2]),
            },
            ..cur
        };
        assert_eq!(
            check_table(&with_replaced("poll_cur_read", bad), FORBIDDEN),
            Err(TableError::LenSumMismatch {
                name: "poll_cur_read",
                sum: 4,
                expected_len: 5
            })
        );
    }

    #[test]
    fn poison_call_row_at_the_rel32_position_is_rejected() {
        let start = find("match_start_call");
        let bad = SiteDef {
            addr: 0x0041_AB2C,
            ..start
        };
        let err = check_table(&with_replaced("match_start_call", bad), FORBIDDEN).unwrap_err();
        assert_eq!(
            err,
            TableError::CallTarget {
                name: "match_start_call",
                computed: 0x0043_BF31,
                original_target: 0x0043_BF30
            }
        );
    }

    #[test]
    fn the_input_device_address_is_the_one_the_poll_reads() {
        // 素の poll は `movzbl 0x4b353f(%ebx)`（`0x0042B8A4`）で席ごとの入力機器を読む。
        assert_eq!(INPUT_DEVICE_P1, 0x004B_353F);
    }

    #[test]
    fn netplay_addresses_appear_in_the_design() {
        const DESIGN: &str = include_str!("../../../docs/Design_Architecture.md");
        let start = DESIGN
            .find("| 値 | どこへ書く | 誰の値 |")
            .expect("設計に揃える値の表が無い");
        let rest = &DESIGN[start..];
        let item = &rest[..rest.find("**CHECK**").expect("表の後の CHECK の段が無い")];
        for (a, b) in [
            (P1_CHAR, P2_CHAR),
            (P1_COLOR, P2_COLOR),
            (P1_LIFE_INIT, P2_LIFE_INIT),
        ] {
            let w = format!("`0x{a:X}`/`0x{b:X}`");
            assert!(item.contains(&w), "表に {w} が無い");
        }
        for w in [
            format!("`0x{MAP:X}`"),
            format!("`0x{DIFFICULTY:X}`"),
            format!("`0x{RNG_ADDR:08X}`"),
            format!("`0x{OPTIONS_BLOCK:08X}`"),
            format!("`[0x{OPTIONS_COPY_PTR:X}]+0x{OPT_CHARGE:X}+席`"),
            format!("`set_state 0x{:08X}`", find(SET_STATE_SITE).addr),
        ] {
            assert!(item.contains(&w), "表の段に {w} が無い");
        }
        let [(a, b), (c, _), (d, _), (e, _)] = OPTION_DIGEST_RANGES;
        let w = format!(
            "`+0x{a:X}`〜`+0x{:X}`・`+0x{c:X}`・`+0x{d:X}`・`+0x{e:X}`",
            b - 1
        );
        assert!(item.contains(&w), "表の段に {w} が無い");
        for (s, e) in &OPTION_DIGEST_RANGES[1..] {
            assert_eq!(e - s, 1, "1 バイトの欄");
        }
        assert_eq!(find(MATCH_START_SITE).addr, 0x0041_AB2B);
    }

    #[test]
    fn select_ranges_appear_in_the_note() {
        const NOTE: &str = include_str!("../../../analysis/notes/th09_select_ranges_M2.md");
        let row = |name: &str| -> &'static str {
            NOTE.lines()
                .find(|l| l.starts_with(&format!("| {name} |")))
                .unwrap_or_else(|| panic!("ノートの表に {name} の行が無い"))
        };
        let cell = |name: &str| row(name).split('|').nth(3).unwrap_or("").trim();
        assert_eq!(cell("キャラ"), format!("0〜{CHARA_MAX}"));
        assert_eq!(cell("色"), format!("0 / {COLOR_MAX}"));
        assert_eq!(cell("初期ライフ"), format!("{LIFE_MIN}〜{LIFE_MAX}"));
        assert_eq!(cell("チャージ型"), format!("0 / {CHARGE_MAX}"));
        assert_eq!(cell("難易度"), format!("0〜{DIFFICULTY_MAX}"));
        assert_eq!(
            cell("マップ"),
            format!(
                "0〜{MAP_MAX} と −{} / −{}",
                -MAP_RANDOM_OF_TWO, -MAP_RANDOM_ANY
            )
        );
        assert!(row("キャラ").contains(&format!("`0x{P1_CHAR:X}` / `0x{P2_CHAR:X}`")));
        assert!(row("色").contains(&format!("`0x{P1_COLOR:X}` / `0x{P2_COLOR:X}`")));
        assert!(
            row("初期ライフ").contains(&format!("`0x{P1_LIFE_INIT:X}` / `0x{P2_LIFE_INIT:X}`"))
        );
        assert!(row("難易度").contains(&format!("`0x{DIFFICULTY:X}`")));
        assert!(row("マップ").contains(&format!("`0x{MAP:X}`")));
    }

    #[test]
    fn select_screen_offsets_appear_in_the_note() {
        const NOTE: &str = include_str!("../../../analysis/notes/th09_select_screens_M2-3.md");
        for w in [
            format!(
                "1P `M+0x{MENU_CHARA_CURSOR:X}`・2P `M+0x{:X}`",
                MENU_CHARA_CURSOR + 4
            ),
            format!(
                "1P `M+0x{MENU_CHARA_DECIDED:X}`・2P `M+0x{:X}`",
                MENU_CHARA_DECIDED + 4
            ),
            format!("`0x{MAP_LIST:X}`"),
            format!("`[M+0x{MENU_SCREEN_ID:X}] == 7` かつ `[M+0x{MENU_SUBSTATE:X}] == 1`"),
            format!("`[M+0x{MENU_TRANSITIONING:X}] != 0`"),
            format!("難易度とマップ: `M+0x{MENU_CURSOR:02X}`"),
        ] {
            assert!(NOTE.contains(&w), "ノートに {w} が無い");
        }
        assert_eq!(MAP_LIST_LEN, 16);
    }

    #[test]
    fn state_control_offsets_appear_in_the_note() {
        const NOTE: &str = include_str!("../../../analysis/notes/th09_state_controls.md");
        for w in [
            format!(
                "`*(f32*)(P+0x{PLAYER_INV_TIMER_F:X}) = {INV_TIMER_VALUE}.0`、`*(i32*)(P+0x{PLAYER_INV_TIMER_I:X}) = {INV_TIMER_VALUE}`"
            ),
            format!("[0x{P1_PLAYER_PTR:X}]`（1P）/ `[0x{P2_PLAYER_PTR:X}]`（2P）"),
            format!("読み込み `[0x{LOADING:X}] == 0`"),
            format!("値: `Player+0x{PLAYER_LIFE:X}`（0〜{LIFE_MAX}）"),
            format!("値: `Player+0x{PLAYER_GAUGE:X}`（0〜{GAUGE_MAX}）"),
            format!("`0x41BCF6 movl $0x43c80000`（= {GAUGE_MAX}.0f）"),
        ] {
            assert!(NOTE.contains(&w), "ノートに {w} が無い");
        }
        assert_eq!(f32::from_bits(0x43C8_0000), GAUGE_MAX as f32);
    }

    #[test]
    fn poison_call_row_without_e8_is_rejected() {
        let call = find("loader_thread_call");
        let bad = SiteDef {
            expected: &[0xE9, 0xCA, 0x62, 0x01, 0x00],
            ..call
        };
        assert_eq!(
            check_table(&with_replaced("loader_thread_call", bad), FORBIDDEN),
            Err(TableError::CallShape {
                name: "loader_thread_call"
            })
        );
    }
}

// ---- ペース B と時刻の記録 ----
// 地点の表 `SITES` とは別に持つ: `SITES` は全行の一致を設置の条件にするので、ここへ足すと vpatch が `0x0042DC44` を書き換えた実行でほかのフックまで全部止まる。
// ペースの行は、ペースと時刻の記録を置くとき（ini の `[pacing]`）だけ `pacing::install` が照合する。検査 `pacing_sites_appear_in_the_m7_note`。

/// ゲームの計算が前提にする x87 の制御語（精度 24 ビット・最近接・例外は全部隠す）。B の始まりで「始める」ときに `fldcw` で置き直す（vpatch の FpuPatch・rev5 #20 と同じ値）。
/// 実機の入口の `fnstcw` は 3,579 件とも `0x007F`。D3D8 の CreateDevice が FPU_PRESERVE 無しで呼ばれるので D3D がこの値にする。
pub const GAME_FPU_CW: u16 = 0x007F;

/// do_frame の先頭（照合だけ）。先頭の call の時点のスタックの形の前提。
pub const PACING_ENTRY_SITE: &str = "pacing_frame_entry";
/// 頭の時計の call（B のときだけ rel32 を書く）。
pub const PACING_HEAD_SITE: &str = "pacing_head_call";
/// B で飛ばす区間（照合だけ）。
pub const PACING_SKIPPED_SITE: &str = "pacing_skipped";
/// B で「始める」ときの着地（照合だけ。アドレスはこの行から引く）。
pub const PACING_RESUME_SITE: &str = "pacing_resume";
/// B で「まだ」のときの着地（照合だけ。アドレスはこの行から引く）。
pub const PACING_EARLY_EXIT_SITE: &str = "pacing_early_exit";
/// Present のラッパの call（時刻の記録。game と b の両方で rel32 を書く）。
pub const PRESENT_SITE: &str = "present_call";

/// ペースと時刻の記録の地点の表（6 行）。
pub const PACING_SITES: &[SiteDef] = &[
    SiteDef {
        name: PACING_ENTRY_SITE,
        addr: 0x0042_DC40,
        expected: &[0x56, 0x57, 0x8B, 0xF1],
        kind: Use::VerifyOnly {
            instr_lens: Some(&[1, 1, 2]),
        },
        source: site_source!("M7 ノート §8-1（th09_disasm.txt L58438〜L58440）"),
    },
    SiteDef {
        name: PACING_HEAD_SITE,
        addr: 0x0042_DC44,
        expected: &[0xE8, 0x47, 0xF0, 0xFF, 0xFF],
        kind: Use::CallRel32 {
            original_target: 0x0042_CC90,
        },
        source: site_source!("M7 ノート §8-1（th09_disasm.txt L58441）"),
    },
    SiteDef {
        name: PACING_SKIPPED_SITE,
        addr: 0x0042_DC49,
        expected: &[
            0xDC, 0x56, 0x34, 0xDD, 0x56, 0x2C, 0xDF, 0xE0, 0xF6, 0xC4, 0x05, 0x7A, 0x03, 0xDD,
            0x56, 0x3C, 0xDC, 0x56, 0x3C, 0xDD, 0x56, 0x34, 0xDF, 0xE0, 0xF6, 0xC4, 0x41, 0x0F,
            0x85, 0x0D, 0x01, 0x00, 0x00, 0xDC, 0x5E, 0x3C, 0xDF, 0xE0, 0xF6, 0xC4, 0x41, 0x75,
            0x19, 0xDD, 0x46, 0x3C, 0xDC, 0x05, 0x78, 0x02, 0x49, 0x00, 0xDD, 0x5E, 0x3C, 0xDD,
            0x46, 0x3C, 0xDC, 0x5E, 0x2C, 0xDF, 0xE0, 0xF6, 0xC4, 0x05, 0x7B, 0xE7,
        ],
        kind: Use::VerifyOnly {
            instr_lens: Some(&[
                3, 3, 2, 3, 2, 3, 3, 3, 2, 3, 6, 3, 2, 3, 2, 3, 6, 3, 3, 3, 2, 3, 2,
            ]),
        },
        source: site_source!("M7 ノート §8-1（th09_disasm.txt L58442〜L58464）"),
    },
    SiteDef {
        name: PACING_RESUME_SITE,
        addr: 0x0042_DC8D,
        expected: &[0x8B, 0x0D, 0x50, 0xC5, 0x4D, 0x00],
        kind: Use::VerifyOnly {
            instr_lens: Some(&[6]),
        },
        source: site_source!("M7 ノート §8-1（th09_disasm.txt L58465）"),
    },
    SiteDef {
        name: PACING_EARLY_EXIT_SITE,
        addr: 0x0042_DD81,
        expected: &[0x5F, 0x33, 0xC0, 0x5E, 0xC3],
        kind: Use::VerifyOnly {
            instr_lens: Some(&[1, 2, 1, 1]),
        },
        source: site_source!("M7 ノート §8-1（th09_disasm.txt L58534〜L58537）"),
    },
    SiteDef {
        name: PRESENT_SITE,
        addr: 0x0042_DD6D,
        expected: &[0xE8, 0xDE, 0xFD, 0xFF, 0xFF],
        kind: Use::CallRel32 {
            original_target: 0x0042_DB50,
        },
        source: site_source!("M7 ノート §8-1（th09_disasm.txt L58526）"),
    },
];

// ---- 画面の文字を花映塚の ascii.anm の文字の絵にする（読むだけ。ゲームには 1 バイトも書かない）----
// 参照する関数: `0x004344E0`〜`0x00434538`（AsciiManager の初期化）・`0x00434540`〜（文字の列を描く）・`0x00434390`（文字の番号 → スプライト）・`0x0043C610` / `0x0043C7A0`（読み込みスロットを返す・読み込みを待つ）・
// `0x0043BCB0`（スプライトの UV と大きさ）・`0x0043C4C0`〜`0x0043C58B`（テクスチャを作る）・`0x0043A0F0` / `0x0043A950`（スプライトを描く）。数値の読み方（実行時）は `overlay.rs` の `probe_game_font`、画素の変換は `higanr_core::overlay`。
/// 読み込みスロット `L`（ascii.anm）へのポインタの置き場 = AsciiManager（`0x004CE458`）の `+0x8288`。`0x004344ED push $0x490f2c`（`"ascii.anm"`）→ `0x004344FA push $1` → `0x004344FC call 0x43c7a0` → `0x00434503 mov %eax,0x8288(%esi)`。
pub const ASCII_ANM_LOADED_PTR: u32 = 0x004C_E458 + 0x8288;
/// AnmManager へのポインタの置き場（`mov 0x4dc550,%ecx` が `0x004344F4` ほか多数）。
pub const ANM_MANAGER_PTR: u32 = 0x004D_C550;
/// AnmManager の中の読み込みスロットの枠の刻み（`0x0043C640` `lea (%ebx,%ebx,8),%eax; shl $5,%eax` = 番号 × 0x120）と、
/// 枠の中で読み込みスロットが始まる位置（`0x0043C682 add $0x24,%esi`）。ascii.anm は番号 1（`push $1`）なので `L = [ANM_MANAGER_PTR] + 0x144`。
/// ポインタ `L` がこの式と食い違うなら、読み込みスロットではない何かを掴んでいる。
pub const ANM_LOADED_STRIDE: u32 = 0x120;
pub const ANM_LOADED_START: u32 = 0x24;
pub const ASCII_ANM_INDEX: u32 = 1;
/// 読み込みスロットの欄。`+0x04` = ファイルの中身（`0x0043C69F mov %eax,0x4(%esi)`。0 なら未読み）・`+0x0C` = スプライトの配列
/// （0x44 バイトずつ。`0x0043C71A mov %eax,0xc(%esi)`）・`+0x18` = 読み込みの進み（`0x0043C7C3 movl $1,0x18(%esi)` で立て、読み込みの
/// スレッドが 0 に戻す。`0x0043C7D0`〜 が 0 になるまで待つ）・`+0x20` = ファイル名（`0x0043C6A2`〜 の文字列の複写）。
pub const LOADED_FILE: u32 = 0x04;
pub const LOADED_SPRITES: u32 = 0x0C;
pub const LOADED_PROGRESS: u32 = 0x18;
pub const LOADED_NAME: u32 = 0x20;
/// 読み込みスロットの名前の期待値（`0x00490F2C` の文字列）。
pub const ASCII_ANM_NAME: &[u8] = b"ascii.anm\0";
/// スプライト 1 つの大きさ（`0x00434399 imul $0x44,%eax`）。文字 `c` のスプライトは番号 `c − 0x20`（`0x00434619 sub $0x20,%ecx`）。
pub const SPRITE_STRIDE: u32 = 0x44;
pub const SPRITE_CHAR_BASE: u32 = 0x20;
/// スプライトの欄。`+0x04` = IDirect3DTexture8*・`+0x08 / +0x0C / +0x10 / +0x14` = テクスチャ上の画素の x0 / y0 / x1 / y1（f32。`0x0043BCD6`〜 が `+0x08 ÷ +0x1C → +0x20`、`+0x0C ÷ +0x18 → +0x24`、
/// `+0x10 ÷ +0x1C → +0x28`、`+0x14 ÷ +0x18 → +0x2C` の UV を作る）・`+0x18` = テクスチャの高さ / `+0x1C` = 幅（f32）・`+0x30` = 高さ / `+0x34` = 幅（f32。ANM の上の画素）。
/// 描くときの四角形の大きさはこの `+0x34 × +0x30` に文字列の拡大を掛けたもの（`0x0043A982`〜 が VM の `+0x28` × `+0x18` を半分にして使う）。
pub const SPRITE_TEXTURE: u32 = 0x04;
pub const SPRITE_X0: u32 = 0x08;
pub const SPRITE_Y0: u32 = 0x0C;
pub const SPRITE_X1: u32 = 0x10;
pub const SPRITE_Y1: u32 = 0x14;
pub const SPRITE_TEX_H: u32 = 0x18;
pub const SPRITE_TEX_W: u32 = 0x1C;
pub const SPRITE_H: u32 = 0x30;
pub const SPRITE_W: u32 = 0x34;
/// IDirect3DTexture8 の GetLevelDesc（IUnknown 3・IDirect3DResource8 8・IDirect3DBaseTexture8 3 の後）。ゲーム自身がテクスチャに `call *0x38(%ecx)` を呼んでいる
/// （`0x0043C8A6`。引数は 段階 0・&desc）。
pub const TEX_GETLEVELDESC_SLOT: u32 = 0x38;
/// D3DSURFACE_DESC（8 語）の中の位置（SDK の並び: Format・Type・Usage・Pool・Size・MultiSampleType・Width・Height）。
pub const SURFACE_DESC_FORMAT: usize = 0;
pub const SURFACE_DESC_WIDTH: usize = 6;
pub const SURFACE_DESC_HEIGHT: usize = 7;

// ---- 文字の描画の後で頂点の流れ 0 を戻す（読むだけ。ゲームには 1 バイトも書かない）----
// ゲームの DrawPrimitive は `0x0043B4BB` の 1 か所だけ（DrawIndexedPrimitive は 0 か所）。その前の `0x0043B449`〜`0x0043B4A6` は、頂点の形の覚えが
// 2 なら SetVertexShader（0x102）・SetStreamSource・段 0 の 2 つを飛ばす。ゲーム自身も DrawPrimitiveUP の後で覚えを 0xFF にする所がある
// （`0x00417447` の DrawPrimitiveUP → `0x00417453 call 0x40e380`）。読み方は `overlay::stream0_to_restore`。
/// AnmManager の頂点の形の覚え（1 バイト。`0x0043B449 cmpb $0x2,0x12886(%esi)`・`0x0043B4A6 movb $0x2`・`0x0040E380 movb $0xff`）。
pub const ANM_VTX_FORMAT: u32 = 0x12886;
/// 覚えがこの値なら、ゲームは流れ 0 に `[AnmManager + ANM_QUAD_VB]` が置いてあるものとして描く。
pub const ANM_VTX_FORMAT_QUAD_VB: u8 = 2;
/// ゲームの頂点バッファ（`0x0043BB4E` の CreateVertexBuffer（長さ 0x50・FVF 0x102・MANAGED）の書き先。`0x0043B465` が読み、
/// `0x0043B478` で SetStreamSource。`0x0042F3D0` が Release して 0 にする）。
pub const ANM_QUAD_VB: u32 = 0x12890;
/// その stride（`0x0043B472 push $0x14`。作った直後の `0x0043BB8B` の SetStreamSource も同じ）。
pub const ANM_QUAD_VB_STRIDE: u32 = 0x14;
// 覚えから頂点バッファまでを 1 回で読む（`stream0_to_restore`）ので、この順で並び、バッファは 4 バイトの境目にあること。
const _: () = assert!(ANM_VTX_FORMAT < ANM_QUAD_VB && ANM_QUAD_VB.is_multiple_of(4));

#[cfg(test)]
mod pacing_tests {
    //! 表の検査（花映塚は使わない）。転記ミスはノートの本文（`include_str!`）と突き合わせる。
    use super::*;
    use crate::sites::{TableError, check_table, hex_bytes};

    const M7: &str = include_str!("../../../analysis/notes/th09_frame_pacing_M7.md");
    const M1A: &str = include_str!("../../../analysis/notes/th09_hook_sites_M1a.md");

    fn m7_section_8_1() -> &'static str {
        let head = "### 8-1 ";
        let start = M7.find(head).expect("M7 のノートに §8-1 が無い");
        let rest = &M7[start + head.len()..];
        &rest[..rest.find("\n### ").unwrap_or(rest.len())]
    }

    #[test]
    fn pacing_sites_appear_in_the_m7_note() {
        let sec = m7_section_8_1();
        let mut calls = 0;
        for d in PACING_SITES {
            let want = format!(
                "| {} | `0x{:08X}` | `{}` |",
                d.name,
                d.addr,
                hex_bytes(d.expected)
            );
            let line = sec
                .lines()
                .find(|l| l.starts_with(&want))
                .unwrap_or_else(|| panic!("§8-1 に {want} の行が無い"));
            if let Use::CallRel32 { original_target } = d.kind {
                assert!(
                    line.contains(&format!("`call 0x{original_target:x}`")),
                    "{}: 行に call 0x{original_target:x} が無い",
                    d.name
                );
                calls += 1;
            }
        }
        assert_eq!(calls, 2, "call 先の差し替えは頭と Present の 2 行");
        let rows = sec
            .lines()
            .filter(|l| l.starts_with("| pacing_") || l.starts_with("| present_"))
            .count();
        assert_eq!(rows, PACING_SITES.len());
        assert!(M1A.contains(&format!(
            "FPU の制御語は 3,579 件とも `0x{GAME_FPU_CW:04X}`"
        )));
    }

    #[test]
    fn screen_flag_appears_in_the_m7_note() {
        let head = "## 12-2. ";
        let start = M7.find(head).expect("M7 のノートに §12-2 が無い");
        let sec = &M7[start..];
        assert!(
            sec.contains(&format!(
                "`[0x{OPTION_WINDOWED:08X}]`（u8）= 設定のブロック `0x004B3488` の `+0xB1`"
            )),
            "§12-2 に 0x{OPTION_WINDOWED:08X} の行が無い"
        );
        assert!(sec.contains(&format!("`cmp %bl,0x{:x}`", OPTION_WINDOWED)));
        assert_eq!(OPTION_WINDOWED, 0x004B_3488 + 0xB1);
    }

    #[test]
    fn pacing_sites_pass_the_table_checks_and_do_not_overlap_sites() {
        assert_eq!(check_table(PACING_SITES, FORBIDDEN), Ok(()));
        let all: Vec<SiteDef> = SITES.iter().chain(PACING_SITES).copied().collect();
        assert_eq!(check_table(&all, FORBIDDEN), Ok(()));
        let row = |n: &str| *PACING_SITES.iter().find(|d| d.name == n).unwrap();
        let (e, h, s, r) = (
            row(PACING_ENTRY_SITE),
            row(PACING_HEAD_SITE),
            row(PACING_SKIPPED_SITE),
            row(PACING_RESUME_SITE),
        );
        assert_eq!(e.range().end, h.addr);
        assert_eq!(h.range().end, s.addr);
        assert_eq!(s.range().end, r.addr);
        assert_eq!(s.expected.len(), 68);
    }

    #[test]
    fn pacing_head_range_is_no_longer_forbidden() {
        let head = PACING_SITES
            .iter()
            .find(|d| d.name == PACING_HEAD_SITE)
            .unwrap();
        let span = AddrRange {
            start: 0x0042_DC40,
            end: 0x0042_DC93,
        };
        assert!(FORBIDDEN.iter().all(|f| !f.overlaps(&span)));
        let mut old = FORBIDDEN.to_vec();
        old.push(AddrRange {
            start: 0x0042_DC45,
            end: 0x0042_DC4C,
        });
        assert_eq!(
            check_table(PACING_SITES, &old),
            Err(TableError::Forbidden {
                name: head.name,
                range: AddrRange {
                    start: 0x0042_DC45,
                    end: 0x0042_DC4C
                }
            })
        );
    }
}

#[cfg(test)]
mod resim_tests {
    //! 表と定数の検査（花映塚は使わない）。転記ミスは、ノートの本文（`include_str!`）と突き合わせる。
    use super::*;
    use crate::sites::{TableError, check_table, hex_bytes};

    const E1: &str = include_str!("../../../analysis/notes/th09_rollback_E1.md");

    fn section_15() -> &'static str {
        let head = "## 15. ";
        let start = E1.find(head).expect("ノートに §15 が無い");
        &E1[start..]
    }

    #[test]
    fn resim_values_appear_in_the_note() {
        let sec = section_15();
        let d = RESIM_SITES
            .iter()
            .find(|d| d.name == RESIM_SCAN_SITE)
            .expect("行が無い");
        assert_eq!(RESIM_SITES.len(), 1, "行は 1 つだけ");
        let want = format!(
            "| {} | `0x{:08X}` | `{}` |",
            d.name,
            d.addr,
            hex_bytes(d.expected)
        );
        let line = sec
            .lines()
            .find(|l| l.starts_with(&want))
            .unwrap_or_else(|| panic!("§15 に {want} の行が無い"));
        let Use::CallRel32 { original_target } = d.kind else {
            panic!("call の行でない");
        };
        assert!(
            line.contains(&format!("`call 0x{original_target:x}`")),
            "{line}"
        );
        let hex8 = |v: u32| format!("`0x{v:08X}`");
        let hexo = |v: u32| format!("`0x{v:X}`");
        let dec = |v: u32| format!("`{v}`");
        for (name, val) in [
            ("SOUND_MANAGER", hex8(SOUND_MANAGER)),
            ("SND_SE_IDS", hexo(SND_SE_IDS)),
            ("SND_SE_COUNTS", hexo(SND_SE_COUNTS)),
            ("SND_SE_SLOTS", dec(SND_SE_SLOTS)),
            ("SND_BGM_QUEUE", hexo(SND_BGM_QUEUE)),
            ("SND_BGM_SLOT_BYTES", hexo(SND_BGM_SLOT_BYTES)),
            ("SND_BGM_SLOTS", dec(SND_BGM_SLOTS)),
            ("ASCII_COUNT", hex8(ASCII_COUNT)),
        ] {
            let prefix = format!("| `{name}` | {val} |");
            assert!(
                sec.lines().any(|l| l.starts_with(&prefix)),
                "§15 に {prefix} の行が無い"
            );
        }
        assert_eq!(SND_SE_IDS + SND_SE_SLOTS * 4, SND_SE_COUNTS);
        assert_eq!(SND_BGM_SLOT_BYTES * SND_BGM_SLOTS, 8308);
        assert_eq!(ASCII_COUNT, 0x004C_E458 + 0x8264);
    }

    #[test]
    fn se_values_appear_in_the_note() {
        let start = E1.find("## 32. ").expect("ノートに §32 が無い");
        let sec = &E1[start..];
        let hex8 = |v: u32| format!("`0x{v:08X}`");
        let hexo = |v: u32| format!("`0x{v:X}`");
        for (name, val) in [
            ("SE_PUSH_FN", hex8(SE_PUSH_FN)),
            ("SE_PUSH_HEAD", format!("`{}`", hex_bytes(&SE_PUSH_HEAD))),
            ("SND_SE_PANS", hexo(SND_SE_PANS)),
            ("SND_SE_PAN_SLOT_BYTES", hexo(SND_SE_PAN_SLOT_BYTES)),
            ("SND_SE_MAX_COUNT", hexo(SND_SE_MAX_COUNT)),
            ("SE_ID_CUTIN", hexo(SE_ID_CUTIN)),
            ("SE_ID_CUTIN_SPELL", hexo(SE_ID_CUTIN_SPELL)),
        ] {
            let prefix = format!("| `{name}` | {val} |");
            assert!(
                sec.lines().any(|l| l.starts_with(&prefix)),
                "§32 に {prefix} の行が無い"
            );
        }
        assert_eq!(SND_SE_COUNTS + SND_SE_SLOTS * 4, SND_SE_PANS);
        assert_eq!(SND_SE_MAX_COUNT * 4, SND_SE_PAN_SLOT_BYTES);
        assert_eq!(SE_PUSH_HEAD.len(), 25);
        assert_eq!(
            &SE_PUSH_HEAD[SE_PUSH_HEAD.len() - 4..],
            &SND_SE_IDS.to_le_bytes()
        );
    }

    #[test]
    fn resim_site_passes_the_table_checks_and_overlaps_nothing() {
        assert_eq!(check_table(RESIM_SITES, FORBIDDEN), Ok(()));
        let all: Vec<SiteDef> = SITES
            .iter()
            .chain(PACING_SITES)
            .chain(ALLOC_SITES)
            .chain(RESIM_SITES)
            .copied()
            .collect();
        assert_eq!(check_table(&all, FORBIDDEN), Ok(()));
        static BAD: [SiteDef; 1] = [SiteDef {
            name: RESIM_SCAN_SITE,
            addr: 0x0042_DCA9,
            expected: &[0xE8, 0x53, 0xEA, 0xFF, 0xFF],
            kind: Use::CallRel32 {
                original_target: 0x0042_C700,
            },
            source: "",
        }];
        assert!(matches!(
            check_table(&BAD, FORBIDDEN),
            Err(TableError::CallTarget { .. })
        ));
        // do_frame の走査の call は、ペースの先頭の call（0x0042DC44）・着地（0x0042DC8D）・Present の call（0x0042DD6D）・描画の
        // detour（0x0042DD51）の間にあり、どれとも 5 バイトも重ならない。
        let r = RESIM_SITES[0].range();
        for other in SITES.iter().chain(PACING_SITES) {
            assert!(!r.overlaps(&other.range()), "{} と重なる", other.name);
        }
    }
}

#[cfg(test)]
mod game_font_tests {
    //! ゲームの文字（ascii.anm）のアドレスの検査（花映塚は使わない）。
    use super::*;

    #[test]
    fn game_font_addresses_match_the_disassembly() {
        assert_eq!(ASCII_ANM_LOADED_PTR, 0x004D_66E0);
        assert_eq!(ANM_MANAGER_PTR, 0x004D_C550);
        assert_eq!(
            ANM_LOADED_STRIDE * ASCII_ANM_INDEX + ANM_LOADED_START,
            0x144
        );
        assert_eq!(
            [LOADED_FILE, LOADED_SPRITES, LOADED_PROGRESS, LOADED_NAME],
            [0x04, 0x0C, 0x18, 0x20]
        );
        assert_eq!(ASCII_ANM_NAME, b"ascii.anm\0");
        assert_eq!((SPRITE_STRIDE, SPRITE_CHAR_BASE), (0x44, 0x20));
        let fields = [
            SPRITE_TEXTURE,
            SPRITE_X0,
            SPRITE_Y0,
            SPRITE_X1,
            SPRITE_Y1,
            SPRITE_TEX_H,
            SPRITE_TEX_W,
            SPRITE_H,
            SPRITE_W,
        ];
        for (i, &a) in fields.iter().enumerate() {
            assert!(a % 4 == 0 && a + 4 <= SPRITE_STRIDE, "{a:#X}");
            assert!(fields[i + 1..].iter().all(|&b| a != b), "{a:#X} が重なる");
        }
        assert_eq!(
            [SPRITE_X0, SPRITE_Y0, SPRITE_X1, SPRITE_Y1],
            [8, 0xC, 0x10, 0x14]
        );
        assert_eq!((SPRITE_TEX_H, SPRITE_TEX_W), (0x18, 0x1C));
        assert_eq!(
            (TEX_GETLEVELDESC_SLOT, D3D8_SURF_RELEASE_SLOT),
            (0x38, 0x08)
        );
        assert!(
            [SURFACE_DESC_FORMAT, SURFACE_DESC_WIDTH, SURFACE_DESC_HEIGHT]
                .iter()
                .all(|&i| i < 8)
        );
    }

    /// 壊れたら落ちる: 頂点の形の覚え・頂点バッファ・stride のどれかを書き写し損ねる（ゲームの頂点バッファでない値を流れ 0 に置く）。
    #[test]
    fn stream0_addresses_match_the_disassembly() {
        assert_eq!(
            (
                ANM_VTX_FORMAT,
                ANM_VTX_FORMAT_QUAD_VB,
                ANM_QUAD_VB,
                ANM_QUAD_VB_STRIDE
            ),
            (0x12886, 2, 0x12890, 0x14)
        );
    }
}

#[cfg(test)]
mod screen_effect_tests {
    //! ScreenEffect の種類の表の検査。花映塚は使わない。
    use super::*;

    #[test]
    fn screen_effect_draws_appear_in_the_m1c_note() {
        const M1C: &str = include_str!("../../../analysis/notes/th09_draw_and_renderonly_M1c.md");
        let line = M1C
            .lines()
            .find(|l| l.contains("0x422EE0"))
            .expect("ノート M1c に ScreenEffect の表の行が無い");
        for (i, k) in SCREEN_EFFECT_KINDS.iter().enumerate() {
            assert_eq!(usize::from(k.kind), i, "種類の並び");
        }
        let draw = |kind: usize| SCREEN_EFFECT_KINDS[kind].draw;
        assert_eq!(draw(0), draw(4), "種類 0 と 4 は同じ描画");
        assert!(
            line.contains(&format!("種類 0・4 → `0x{:06X}`", draw(0))),
            "{line}"
        );
        for k in [2usize, 3, 5, 6] {
            assert!(
                line.contains(&format!("{k} → `0x{:06X}`", draw(k))),
                "種類 {k}: {line}"
            );
        }
        assert!(line.contains("種類 1・7 は描画なし"), "{line}");
        assert_eq!((draw(1), draw(7)), (0, 0));
        assert!(SCREEN_EFFECT_KINDS.iter().all(|k| k.update != 0));
        for k in SCREEN_EFFECT_KINDS {
            assert!(is_screen_effect_fn(k.update), "{k:?}");
            assert!(k.draw == 0 || is_screen_effect_fn(k.draw), "{k:?}");
            assert!(
                SCREEN_EFFECT_KINDS.iter().all(|o| o.draw != k.update),
                "{k:?}"
            );
        }
        assert!(!is_screen_effect_fn(0));
        assert!(!is_screen_effect_fn(0x0042_0190));
        assert!(!is_screen_effect_fn(0x0043_1540));
    }
}

#[cfg(test)]
mod render_reset_tests {
    //! 描画が消す欄の表の検査。花映塚は使わない。
    use super::*;

    fn instrs() -> Vec<(u32, &'static [u8])> {
        let mut v = Vec::new();
        let mut at = 0usize;
        for &n in STAGE_TINT_RESET_LENS {
            v.push((
                STAGE_TINT_RESET_AT + at as u32,
                &STAGE_TINT_RESET_CODE[at..at + n],
            ));
            at += n;
        }
        assert_eq!(
            at,
            STAGE_TINT_RESET_CODE.len(),
            "命令の長さの和がバイト列の長さと違う"
        );
        v
    }

    #[test]
    fn stage_tint_reset_matches_the_draw_code() {
        const NOTE: &str = include_str!("../../../analysis/notes/th09_draw_and_renderonly_M1c.md");
        let mut al: Option<u8> = None;
        let mut window: [Option<u8>; 4] = [None; 4];
        let disp = |b: &[u8]| u32::from_le_bytes([b[2], b[3], b[4], b[5]]);
        for (addr, b) in instrs() {
            let (at, v) = match b {
                // mov $imm8,%al
                [0xB0, imm] => {
                    al = Some(*imm);
                    (None, 0)
                }
                // movb $imm8,disp32(%edi)
                [0xC6, 0x87, _, _, _, _, imm] => (Some(disp(b)), *imm),
                // mov %al,disp32(%edi)
                [0x88, 0x87, _, _, _, _] => (Some(disp(b)), al.expect("al を置く前に書く")),
                _ => panic!("{addr:#X} の命令 {b:02X?} を読めない"),
            };
            if let Some(d) = at {
                let i = d.checked_sub(STAGE_TINT).expect("欄の前へ書く") as usize;
                assert!(i < 4, "欄の後ろ {d:#X} へ書く");
                window[i] = Some(v);
            }
            let hex: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
            let line = format!("{addr:x}: {}", hex.join(" "));
            assert!(NOTE.contains(&line), "ノート §1-7 に「{line}」が無い");
        }
        let written: Vec<u8> = window
            .iter()
            .map(|w| w.expect("欄の一部を書かない"))
            .collect();
        assert_eq!(written, STAGE_TINT_AFTER, "描画の後の値が命令と違う");
        for r in RENDER_RESETS {
            assert_eq!(
                (r.offset, r.after, r.code_at, r.code),
                (
                    STAGE_TINT,
                    &STAGE_TINT_AFTER[..],
                    STAGE_TINT_RESET_AT,
                    STAGE_TINT_RESET_CODE
                ),
                "{}",
                r.block
            );
        }
    }

    #[test]
    fn render_reset_rows_name_both_stage_blocks_once() {
        let names: Vec<&str> = RENDER_RESETS.iter().map(|r| r.block).collect();
        assert_eq!(names, STAGE_BLOCKS, "1P・2P の Stage の順に 1 行ずつ");
        for r in RENDER_RESETS {
            let b = ROLLBACK_BLOCKS
                .iter()
                .find(|b| b.name == r.block)
                .unwrap_or_else(|| panic!("{} がコピーの表に無い", r.block));
            assert!(
                r.offset as usize + r.after.len() <= b.size as usize,
                "{} の欄がはみ出す",
                r.block
            );
            assert_eq!(b.size, 0x65C4, "{}", r.block);
        }
    }
}

// ---- 被弾音を確定まで待つ（rollback の SE）----

/// 被弾の SE の id。被弾処理 `0x41E420` の中の `0x0041E449 push $0x4` → `0x0041E450 call 0x43e380`（`0x43E380` も `0x43E2F0` と同じ待ち行列 `SOUND_MANAGER +0x620` へ積む）。
/// id を変数で渡す呼び出し（`0x40ACC5`・`0x41F45A`）から id 4 が鳴らないとは言い切れない。
pub const SE_ID_HIT: u32 = 0x04;
/// ラストライフに落ちる被弾で続けて積む SE の id（`0x0041E61A push $0x30` → `0x0041E621 call 0x43e2f0`）。警報音かは耳で未確認。
pub const SE_ID_LAST_LIFE: u32 = 0x30;

// ---- ウィンドウモードのウィンドウを作る呼び出し（ini の `[window]` で大きさ・位置・枠・最前面を決める）----
// 実物 th09.exe の `0x42D597`〜`0x42D5C8` の 49 バイトと突き合わせた。`SITES` には入れない: DllMain の中（メインスレッドが走り出す前。`window_win::install`）で独立に照合して書く。合わなければ何も書かず、ウィンドウは今のまま。
// 全画面の呼び出し `0x42D56E` は触らない。

/// ウィンドウモードの枝の `call *0x48e1fc`（`CreateWindowExA`。6 バイト `FF 15 FC E1 48 00`）。
pub const WINDOW_CREATE_CALL: u32 = 0x0042_D5C2;
/// `CreateWindowExA` の IAT（`.rdata`）。差し替えた関数が元の関数を呼ぶとき、呼ぶ時点の値を読む。
pub const WINDOW_CREATE_IAT: u32 = 0x0048_E1FC;
/// 照合する範囲（引数の積み込みの全部と、最後の `call`）の行の名前。
pub const WINDOW_CREATE_SITE: &str = "window_create_call";
/// 照合する範囲の先頭から `WINDOW_CREATE_CALL` までのバイト数。
pub const WINDOW_CREATE_CALL_OFFSET: usize = 43;
/// ウィンドウを作る呼び出しの前後の照合の表（1 行。49 バイト）。
pub const WINDOW_CREATE_SITES: &[SiteDef] = &[SiteDef {
    name: WINDOW_CREATE_SITE,
    addr: 0x0042_D597,
    expected: &[
        0x6A, 0x00, 0x53, 0x6A, 0x00, 0x6A, 0x00, 0x8D, 0x84, 0x48, 0xE0, 0x01, 0x00, 0x00, 0x50,
        0x57, 0x68, 0x00, 0x00, 0x00, 0x80, 0x68, 0x00, 0x00, 0x00, 0x80, 0x68, 0x00, 0x00, 0x0A,
        0x10, 0x68, 0xE4, 0x01, 0x49, 0x00, 0x68, 0x1C, 0x02, 0x49, 0x00, 0x6A, 0x00, 0xFF, 0x15,
        0xFC, 0xE1, 0x48, 0x00,
    ],
    kind: Use::VerifyOnly {
        instr_lens: Some(&[2, 1, 2, 2, 7, 1, 1, 5, 5, 5, 5, 5, 2, 6]),
    },
    source: site_source!("ノート th09_window_size.md §2（th09_disasm.txt L57870〜L57883）"),
}];
// 照合する範囲の終わりの 6 バイトが、差し替える call そのもの（建てるときに確かめる）。
const _: () =
    assert!(WINDOW_CREATE_SITES[0].addr + WINDOW_CREATE_CALL_OFFSET as u32 == WINDOW_CREATE_CALL);
const _: () = assert!(WINDOW_CREATE_SITES[0].expected.len() == WINDOW_CREATE_CALL_OFFSET + 6);

#[cfg(test)]
mod window_size_tests {
    //! ウィンドウを作る呼び出しの表の検査（花映塚は使わない）。転記ミスはノートの本文（`include_str!`）と突き合わせる。
    use super::*;

    const NOTE: &str = include_str!("../../../analysis/notes/th09_window_size.md");

    #[test]
    fn window_create_rows_appear_in_the_note() {
        let s = &WINDOW_CREATE_SITES[0];
        let lens = match s.kind {
            Use::VerifyOnly {
                instr_lens: Some(l),
            } => l,
            _ => panic!("境目つきの照合だけの行のはず"),
        };
        assert_eq!(
            lens.iter().map(|&n| usize::from(n)).sum::<usize>(),
            s.expected.len()
        );
        let mut at = 0usize;
        for &n in lens {
            let b = &s.expected[at..at + usize::from(n)];
            let hex: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
            let line = format!("{:x}: {}", s.addr + at as u32, hex.join(" "));
            assert!(NOTE.contains(&line), "ノート §2 に「{line}」が無い");
            at += usize::from(n);
        }
        // 差し替える命令は `call *0x48e1fc`（`FF 15` + IAT のアドレス。表とは別に書いた転記）。
        let call = [0xFF, 0x15, 0xFC, 0xE1, 0x48, 0x00];
        assert_eq!(&s.expected[WINDOW_CREATE_CALL_OFFSET..], &call);
        assert_eq!(
            u32::from_le_bytes([call[2], call[3], call[4], call[5]]),
            WINDOW_CREATE_IAT
        );
        for needle in ["0x42D5C2", "0x0048E1FC", "0x42D56E"] {
            assert!(NOTE.contains(needle), "ノートに {needle} が無い");
        }
        assert_eq!(s.name, WINDOW_CREATE_SITE);
    }

    #[test]
    fn window_create_range_overlaps_nothing_and_leaves_fullscreen_alone() {
        let r = WINDOW_CREATE_SITES[0].range();
        for tables in [
            SITES,
            POLL_SITES,
            PORTRAIT_SITES,
            ALLOC_SITES,
            PACING_SITES,
            RESIM_SITES,
        ] {
            for other in tables {
                assert!(!r.overlaps(&other.range()), "{} と重なる", other.name);
            }
        }
        for f in FORBIDDEN {
            assert!(!r.overlaps(f), "{f:?} と重なる");
        }
        let fullscreen = AddrRange {
            start: 0x0042_D56E,
            end: 0x0042_D574,
        };
        assert!(!r.overlaps(&fullscreen));
    }
}

// ---- パッドの十字キー（POV ハット）を方向のビットに足す入口 ----
// ゲームが読まない十字キーを、パッド 1 台を読む関数 `0x0042B410` の戻り値に OR する。`SITES` に入れない。この表は DLL の初期化（`pad_extra_win::on_attach`）が別に照合して書く
// （6 行が 1 つでも合わなければ 1 バイトも書かない）。検査 `pad_rows_match_the_note_and_the_exe_shape`。

/// パッド 1 台を読む関数（ecx = 席、edx = デバイス番号、スタックに 1 語、`ret 4`）。入口は元のこの関数を同じ引数で呼ぶ。
pub const PAD_READ_FN: u32 = 0x0042_B410;
/// DirectInput のデバイス（IDirectInputDevice8 へのポインタ）の表。`[PAD_DEVICE_SLOTS + 4 × デバイス番号]`（`0x0042B419 mov 0x4b3114,%eax`・
/// `0x0042B43C mov 0x4b3118,%eax`・`0x0042B449 mov 0x4b3114(,%esi,4),%eax`）。2 つとも 0 でなければゲームは DirectInput の経路で読み、
/// どちらかが 0 なら winmm（`joyGetPosEx`）で読む。
pub const PAD_DEVICE_SLOTS: u32 = 0x004B_3114;
/// 上の表の 2 つ目（1 つ目の 4 バイト後）。
pub const PAD_DEVICE_SLOT_1: u32 = PAD_DEVICE_SLOTS + 4;
/// IDirectInputDevice8 の vtable の GetDeviceState の位置（バイト）。ゲームの `0x0042B4BF call *0x24(%edx)`。
pub const DI_GET_DEVICE_STATE_OFFSET: u32 = 0x24;

/// 4 つの呼び出しと、呼び先の先頭・最後の `ret 4` の 6 行。`CallRel32` の 4 行は rel32 を入口へ向ける行、`VerifyOnly` の 2 行は呼び出しの約束
/// （先頭の `push ebp; mov ebp,esp; sub esp,0x148` と、最後の `mov esp,ebp; pop ebp; ret 4`）が実物でも同じかの照合だけ。
pub const PAD_SITES: &[SiteDef] = &[
    SiteDef {
        name: "pad_call_poll_merged_1p",
        addr: 0x0042_BD7F,
        expected: &[0xE8, 0x8C, 0xF6, 0xFF, 0xFF],
        kind: Use::CallRel32 {
            original_target: PAD_READ_FN,
        },
        source: site_source!("ノート th09_pad_pov.md §2（th09_disasm.txt L55495）"),
    },
    SiteDef {
        name: "pad_call_poll",
        addr: 0x0042_BDB2,
        expected: &[0xE8, 0x59, 0xF6, 0xFF, 0xFF],
        kind: Use::CallRel32 {
            original_target: PAD_READ_FN,
        },
        source: site_source!("ノート th09_pad_pov.md §2（th09_disasm.txt L55514）"),
    },
    SiteDef {
        name: "pad_call_local_1p",
        addr: 0x0042_C140,
        expected: &[0xE8, 0xCB, 0xF2, 0xFF, 0xFF],
        kind: Use::CallRel32 {
            original_target: PAD_READ_FN,
        },
        source: site_source!("ノート th09_pad_pov.md §2（th09_disasm.txt L55815）"),
    },
    SiteDef {
        name: "pad_call_local_2p",
        addr: 0x0042_C161,
        expected: &[0xE8, 0xAA, 0xF2, 0xFF, 0xFF],
        kind: Use::CallRel32 {
            original_target: PAD_READ_FN,
        },
        source: site_source!("ノート th09_pad_pov.md §2（th09_disasm.txt L55816）"),
    },
    SiteDef {
        name: "pad_fn_head",
        addr: PAD_READ_FN,
        expected: &[0x55, 0x8B, 0xEC, 0x81, 0xEC, 0x48, 0x01, 0x00, 0x00],
        kind: Use::VerifyOnly {
            instr_lens: Some(&[1, 2, 6]),
        },
        source: site_source!("ノート th09_pad_pov.md §1（th09_disasm.txt L54743〜54745）"),
    },
    SiteDef {
        name: "pad_fn_ret",
        addr: 0x0042_B732,
        expected: &[0x8B, 0xE5, 0x5D, 0xC2, 0x04, 0x00],
        kind: Use::VerifyOnly {
            instr_lens: Some(&[2, 1, 3]),
        },
        source: site_source!("ノート th09_pad_pov.md §1（th09_disasm.txt L54986〜54988）"),
    },
];

#[cfg(test)]
mod pad_tests {
    use super::*;
    use crate::sites::check_table;

    const PAD_NOTE: &str = include_str!("../../../analysis/notes/th09_pad_pov.md");

    #[test]
    fn pad_rows_match_the_note_and_the_exe_shape() {
        assert_eq!(PAD_SITES.len(), 6);
        let mut calls = 0;
        for d in PAD_SITES {
            let hex = d
                .expected
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ");
            let want = format!("| {} | `0x{:08X}` | `{hex}` |", d.name, d.addr);
            assert!(
                PAD_NOTE.lines().any(|l| l.starts_with(&want)),
                "ノート th09_pad_pov.md §2 に {want} の行が無い"
            );
            if let Use::CallRel32 { original_target } = d.kind {
                calls += 1;
                let bytes: [u8; 5] = d.expected.try_into().expect("E8 rel32 の 5 バイト");
                assert_eq!(
                    higanr_core::patch::call_target(d.addr, &bytes),
                    Some(PAD_READ_FN),
                    "{} の rel32 が PAD_READ_FN を指さない",
                    d.name
                );
                assert_eq!(original_target, PAD_READ_FN);
            }
        }
        assert_eq!(calls, 4, "書く行は呼び出し 4 つ");
        let ret = PAD_SITES
            .iter()
            .find(|d| d.name == "pad_fn_ret")
            .expect("pad_fn_ret");
        assert_eq!(&ret.expected[3..], &[0xC2, 0x04, 0x00], "ret 4");
        let head = PAD_SITES
            .iter()
            .find(|d| d.name == "pad_fn_head")
            .expect("pad_fn_head");
        assert_eq!(head.addr, PAD_READ_FN);
        let note = PAD_NOTE.to_lowercase();
        assert!(note.contains(&format!("0x{PAD_DEVICE_SLOTS:08x}")) || note.contains("0x4b3114"));
        assert_eq!(PAD_DEVICE_SLOT_1, 0x004B_3118);
        assert!(note.contains("0x4b3118"));
        assert!(note.contains(&format!("call *0x{DI_GET_DEVICE_STATE_OFFSET:x}(%edx)")));
    }

    #[test]
    fn pad_rows_overlap_nothing_and_pass_check_table() {
        assert_eq!(check_table(PAD_SITES, FORBIDDEN), Ok(()));
        assert!(
            SITES.iter().all(|s| !s.name.starts_with("pad_")),
            "SITES（14 行）に入れない"
        );
        let all: Vec<SiteDef> = SITES
            .iter()
            .chain(POLL_SITES)
            .chain(PORTRAIT_SITES)
            .chain(PACING_SITES)
            .chain(ALLOC_SITES)
            .chain(RESIM_SITES)
            .chain(CALLOC_SITES)
            .chain(PAD_SITES)
            .copied()
            .collect();
        assert_eq!(check_table(&all, FORBIDDEN), Ok(()));
    }
}
