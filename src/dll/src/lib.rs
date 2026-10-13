//! HiganRTool.dll —— th09.exe に注入される DLL の入口。
//!
//! 注入されたら別スレッドで自分の exe の同定と、地点の表（`th09_addr.rs`）の各地点の期待バイト列の照合（読むだけ）をし、
//! **全部が通ったときだけ** `0x0043112D` の call の rel32 の 4 バイトを自前の入口（`stub.rs`）へ向ける。入口は回数を数え、
//! tick ごとの記録を 1 件（1024 バイト。先頭 64 バイトとコピーの表の範囲）DLL の中のリングバッファに書いて元の飛び先へ飛ぶ（素のアセンブリ。
//! ゲームのスレッドで Rust のコードは走らない）。設置の後は最大 15 分、100 ms ごとにリングバッファを読み、1 秒ごとに `_ticks.csv` /
//! `_changes.csv` / `_snap.bin` へ追記し、1 秒ごとの回数と 60 秒ごとの集計をログに書いてスレッドを終える（フックは残る）。
//! 別のスレッドが約 1 ms ごとにメニューとライフを読み、変化を `_menu.csv` へ追記する。ログとほかのファイルは DLL と同じ
//! フォルダの `logs\` に 1 回ごとに別名で書く。
//!
//! DllMain はローダーロックの下で走るので、ミューテックスを取ってスレッドを起こすだけで待たない。フックの設置も
//! ネットワークもウィンドウの作成もしない。例外はパッドの十字キーの入口（`pad_extra_win::on_attach`）で、ここで 4 つの rel32 を書く。
//! ローダーはゲームを `CREATE_SUSPENDED` で作り `LoadLibraryW` が戻るまで主スレッドを走らせないので、ゲームのコードを
//! 誰も実行していない唯一の時点で、読み取りのスレッドの設置と互いのスレッドを止め合うこともない。
//!
//! DLL のフォルダに `script.txt` があれば、設置の条件が通った後で読み、`logs\` へ移し、台本として置く。2 つのフックを
//! 書いた後で入力の入口の分かれ経路を開き、ゲームのスレッドで台本を 1 tick ずつ進めて feed する（`script_win.rs` →
//! `scripted.rs`）。描画の入口は撮影の要求があればバックバッファを置き場へコピーする（`shot_win.rs`）。読み取りのスレッドは
//! feed の記録を模型と照らし（`feeds.rs`）、コピーした絵を PNG にする（`shots.rs`・`script_watch.rs`）。
//!
//! ネットワークは、受信のリングが `net_ring.rs`、ソケット・受信スレッド・ガードが `net_udp.rs`、入力の地点の判断が `netplay.rs`。
//!
//! unsafe は次のモジュールに閉じ込める: `win.rs`（Win32 とメモリの読み取り）・`hook_win.rs`（スレッドの停止と書き込み）・
//! `record_win.rs`（TSC・QPC・CPUID）・`menu_win.rs`（`ReadProcessMemory` と待機タイマー）・`script_win.rs`（台本の入れ物・
//! ゲームのメモリの直の読み・feed とローカル入力の呼び出し・`MoveFileExW`）・`shot_win.rs`（D3D8 の vtable の呼び出し・
//! 撮影の置き場）・`stub.rs` と `draw.rs`（入口のアセンブリ）・`alloc_win.rs`（確保・解放の数えの入口のアセンブリ）・
//! `net_win.rs`（受信のイベントと待機タイマー・SSDP を送るインターフェースの指定）・`replay_save_win.rs`（保存関数を呼ぶ素のアセンブリ・ゲームのメモリの
//! 読み書き・`CreateFileA` でのファイルの同一性の確かめ）・`notify_win.rs`（自分のプロセスのウィンドウを探す・`FlashWindowEx`・
//! `PlaySoundW`）・`clipboard_win.rs`（クリップボード）・`room_win.rs`（WinHTTP の WebSocket・検査の SHA-1）・
//! `text_input_win.rs`（ゲームのウィンドウの手続きの差し替えと IME の切り離し）・`digit_keys_win.rs`（`GetAsyncKeyState`）・
//! `pad_extra_win.rs`（ゲームが呼ぶ入口・DirectInput と winmm の読み）・`ui_lang_win.rs`（`GetUserDefaultUILanguage`）。
//! 検査のときだけ `test_alloc.rs` も。
//! 他のモジュールは `#![forbid(unsafe_code)]`。

mod alloc_count;
mod alloc_win;
// ゲームの中のメニューが接続コードをコピーする・貼る部分（本物のクリップボードの unsafe は `clipboard_win.rs` の中だけ）。
mod clipboard;
mod clipboard_win;
mod copies;
mod detour;
// 数字キー（上段の 0〜9）で遅れの量を替える入口: 押した瞬間の取り方（pure）と、キーの読み（`GetAsyncKeyState`。unsafe はこの中だけ）。
mod digit_keys;
mod digit_keys_win;
mod draw;
mod feeds;
mod hook;
mod hook_win;
mod menu;
mod menu_win;
mod names;
mod net_code;
// ゲーム内メニューの見張り（`code_side = game`）。
mod net_code_game;
mod net_config;
mod net_game;
mod net_portrait;
mod net_ring;
// ルームサーバーの運び。unsafe は `room_win.rs`（WinHTTP の WebSocket）の中だけ。見張り（`net_code_game`）から対戦の「ルームマッチ」で使う。
// 製品のビルドで使わない関数が残るので dead_code を許す。
#[cfg_attr(not(test), allow(dead_code))]
mod net_room;
mod net_select;
// ゲームの中のメニューの、ゲームのスレッドの側と描く行の盤。
mod net_udp;
mod net_ui;
mod net_watch;
mod net_win;
mod netplay;
// 繋がった知らせ（音と点滅。unsafe はこの中だけ）。
mod notify_win;
mod observe;
// 文字の描画（unsafe は `overlay_win.rs`: D3D8 の vtable の呼び出し・GDI・文字の表の置き場）。
mod overlay;
mod overlay_win;
// ペース B と時刻のフック。unsafe は `pacing_win.rs`（QPC・入口のアセンブリ・待機タイマー）にもある。
mod pacing;
mod pacing_win;
// パッドの十字キー（POV ハット）を方向に足す入口。判断と設置の手順は `pad_extra.rs`（pure）、本物の読み・ゲームが呼ぶ入口・
// `DllMain` からの設置は `pad_extra_win.rs`（unsafe はこの中だけ）。
mod log_mode;
mod main_log;
mod match_events;
mod pad_extra;
mod pad_extra_win;
// メニューで決めたプレイヤー名を覚えるファイル（pure。起動のときと、名前を決めたときの別のスレッドだけが読み書きする）。
mod player_name_file;
mod probe;
mod record_win;
mod render_only;
// 自動保存（unsafe は `replay_save_win.rs` の中だけ）。
mod replay_save;
mod replay_save_win;
// 描画用の VM の欄の診断と除外（pure）。
mod rollback_draw;
// rollback のモードの製品の経路の部品（pure。Win32 とゲームのメモリは `rollback_play_win.rs`）。`netplay/rollback.rs` が状態機械を
// 進め、`script_win.rs` が走査の包みのフック・入力の地点・Present の後から呼ぶ。
mod rollback_play;
mod rollback_play_win;
mod rollback_probe;
#[cfg(test)]
mod rollback_testkit;
// 進め直しを 1 回の描画の中で行う形（pure。入口のアセンブリは `rollback_win.rs`）。
mod rollback_resim;
mod rollback_win;
// 前に使ったルーム名を覚えるファイル（ルーム名の入力の初めの値。pure。見張りのスレッドと起動のときだけが読み書きする）。
mod room_last;
// ルームサーバーの WebSocket（WinHTTP。unsafe はこの中だけ）。`net_room` から使う（製品で使わない関数が残るので dead_code を許す）。
#[cfg_attr(not(test), allow(dead_code))]
mod room_win;
// 配るバージョンで ini が無いとき、配る ini と同じ中身を DLL のフォルダへ書き出す（起動のとき 1 回）。
mod ini_seed;
mod script_watch;
mod script_win;
mod scripted;
mod session;
// ローダーのウィンドウ（status.txt）に出す文の日本語・英語（pure）。
mod shot_win;
mod shots;
mod show_text;
mod sites;
// 観戦のホストの側: ゲームのスレッドが書くリングバッファと、受信スレッドが送る側。
mod spectate_host;
// インターネット越しの観戦（観戦者の側）: ゲーム内のメニューの観戦の「ルームマッチ」を決めた人のゲームが、観戦用のルームへ入って
// 対戦者の観戦用のコードを受け、返しを出して JOIN を始めさせる。
mod spectate_join;
// インターネット越しの観戦（配る側）: マッチのホストが観戦用のルームで観戦者を待ち、返しを受けて穴あけを頼む。
mod spectate_offer;
mod spectate_ring;
#[cfg(test)]
mod spectate_testkit;
// 観戦者の側: ホストの記録を組み立てて観戦者のゲームへ入れる判断と、観戦用のコードを読む見張り。
mod spectate_watch;
mod spectate_watch_code;
// 台本のマッチの状態の操作（無敵・ライフ・ゲージ。pure。書き込みの部分は `script_win.rs`）。
mod state_ops;
mod stub;
#[cfg(test)]
mod test_alloc;
// ルーム名の入力がウィンドウから文字を受ける仕組み（ウィンドウの手続きの差し替えと IME。unsafe はこの中だけ）。
mod text_input_win;
mod th09_addr;
mod ticks;
// 画面の言語（日本語・英語）の今の値と ini への書き戻し（pure）。OS の UI の言語の読みは `ui_lang_win.rs`（unsafe はこの中だけ）。
mod ui_lang;
mod ui_lang_win;
// 試験のまとめ（本ログの最後の節。pure）。
mod trial_summary;
// 家のルーターへ UPnP でポートの割り当てを頼む（通信は LAN の中だけ。SSDP を送るインターフェースの指定だけ `net_win.rs` の unsafe）。
mod upnp;
mod watch_feed;
mod win;
// ウィンドウモードのウィンドウを作る呼び出しの差し替え（ini の `[window]`。DllMain の中で書くので、unsafe はこの中だけ）。
mod window_win;

use core::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

use windows_sys::Win32::Foundation::HINSTANCE;
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::core::BOOL;

const TRUE: BOOL = 1;
const FALSE: BOOL = 0;

/// DLL の入口。OS のローダーだけが呼ぶ。`extern "system"` は 32bit の Windows では stdcall。
/// パニックは `catch_unwind` で受けて外へ出さず、受けたら FALSE を返す（ローダーは `LoadLibraryW` が 0 と見て、
/// ゲームを始めずに終える）。
#[unsafe(no_mangle)]
#[allow(non_snake_case)] // 名前は Win32 が決めたもの
pub extern "system" fn DllMain(module: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason != DLL_PROCESS_ATTACH {
        return TRUE;
    }
    match catch_unwind(AssertUnwindSafe(|| win::on_process_attach(module))) {
        Ok(true) => {
            // 十字キーの入口は、ゲームの主スレッドがまだ走っていない今に書く。失敗しても読み取りのスレッドは起きているので、
            // DLL の読み込みは成功のまま返す（パッドの十字キーが効かないだけ）。
            let _ = catch_unwind(AssertUnwindSafe(|| pad_extra_win::on_attach(module)));
            TRUE
        }
        Ok(false) | Err(_) => FALSE,
    }
}
