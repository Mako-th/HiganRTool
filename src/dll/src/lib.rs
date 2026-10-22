//! HiganRTool.dll —— th09.exe に注入される DLL の入口。
//!
//! 注入されたら別スレッドで自分の exe の同定と、地点の表（`th09_addr.rs`）の各地点の期待バイト列の照合（読むだけ）をし、**全部が通ったときだけ** `0x0043112D` の call の rel32 の 4 バイトを自前の入口（`stub.rs`）へ向ける。
//! 入口は回数を数え、tick ごとの記録を DLL の中のリングバッファに書いて元の飛び先へ飛ぶ（素のアセンブリ。ゲームのスレッドで Rust のコードは走らない）。設置の後は読み取りのスレッドがリングバッファを読み、`_ticks.csv` などへ追記してログに書く。
//! ログとほかのファイルは DLL と同じフォルダの `logs\` に 1 回ごとに別名で書く。
//!
//! DllMain はローダーロックの下で走るので、ミューテックスを取ってスレッドを作るだけで待たない。フックの設置もネットワークもウィンドウの作成もしない。例外はパッドの十字キーの入口（`pad_extra_win::on_attach`）などで、ここで rel32 を書く。
//! ローダーはゲームを `CREATE_SUSPENDED` で作り `LoadLibraryW` が戻るまで主スレッドを走らせないので、ゲームのコードを誰も実行していない唯一の時点で書ける。
//! unsafe は `*_win.rs`・`stub.rs`・`draw.rs` など Win32 とアセンブリの層に閉じ込め、他のモジュールは `#![forbid(unsafe_code)]`。

// ネットワークアダプターの一覧（`bind = auto` が経路を引けないとき・LAN のホストが自分のアドレスを見せるとき。unsafe はこの中だけ）。
mod adapters_win;
mod alloc_count;
mod alloc_win;
// 起動直後のリフレッシュレートの計測の BeginScene の呼びの差し替え（デバイスが失われていたら計測を打ち切る。DllMain の中で書くので、unsafe はこの中だけ）。
mod calib_guard_win;
// ゲームの中のメニューが接続コードをコピーする・貼る部分（本物のクリップボードの unsafe は `clipboard_win.rs` の中だけ）。
mod clipboard;
mod clipboard_win;
mod copies;
mod crash_win;
// 読み込まれている d3d8.dll の場所とバージョン情報（本ログの 1 行。unsafe はこの中だけ）。
mod d3d8_info_win;
mod detour;
// 数字キー（上段の 0〜9）で遅れの量を替える入口: 押した瞬間の取り方（pure）と、キーの読み（`GetAsyncKeyState`。unsafe はこの中だけ）。
mod digit_keys;
mod digit_keys_win;
mod draw;
mod feeds;
mod hook;
mod hook_win;
// LAN 対戦の申し込みの運び手（std のソケットだけ。unsafe 無し）。
mod lan_link;
// 接続ごとの本ログの行（接続した行・stall の内訳・RTT・観戦者の出入り。pure）。
mod link_log;
mod menu;
// ゲームのメニューから保存するリプレイ（長い試合で落ちる不具合の避け方と、HiganRTool の印）。判断は `menu_save.rs`（pure）、入口と設置と印を足すスレッドは
// `menu_save_win.rs`（unsafe はこの中だけ）。
mod menu_save;
mod menu_save_win;
mod menu_win;
mod names;
mod net_code;
// ゲーム内メニューの見張り（`code_side = game`）。
mod net_code_game;
mod net_config;
mod net_game;
mod net_portrait;
mod net_ring;
// ルームサーバーとの送受信を受け持つ部分。unsafe は `room_win.rs`（WinHTTP の WebSocket）の中だけ。見張り（`net_code_game`）から対戦の「ルームマッチ」で使う。
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
// 接続した知らせ（音と点滅。unsafe はこの中だけ）。
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
// メインループの TestCooperativeLevel の差し替え（デバイスが失われている間は 10 ms ずつ寝る。DllMain の中で書くので、unsafe はこの中だけ）。
mod lost_sleep_win;
mod main_log;
mod match_events;
mod pad_extra;
mod pad_extra_win;
mod probe;
mod record_win;
mod render_only;
// 自分の PC の VPN のネットワークの控え（LAN のアドレスの入力が使う。unsafe 無し）。
mod vpn_snap;
// 自動保存（unsafe は `replay_save_win.rs` の中だけ）。
mod replay_save;
mod replay_save_win;
// 素の花映塚でのリプレイの再生の見張り（判断は `replay_watch.rs`（pure）、ゲームのスレッドと本ログへの受け渡しは `replay_watch_win.rs`）。
mod replay_watch;
mod replay_watch_win;
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
// ルームサーバーの WebSocket（WinHTTP。unsafe はこの中だけ）。`net_room` から使う（製品で使わない関数が残るので dead_code を許す）。
#[cfg_attr(not(test), allow(dead_code))]
mod room_win;
// 利用者へ配るバージョンで ini が無いとき、配る ini と同じ中身を DLL のフォルダへ書き出す（起動のとき 1 回）。
mod ini_seed;
// メニューで決めた設定を ini の該当の 1 行へ書き戻す（別のスレッド）と、以前のバージョンの別ファイルの引き継ぎ（pure。unsafe 無し）。
mod ini_save;
mod script_watch;
mod script_win;
mod scripted;
mod session;
// 自分の PC にだけゲーム自身のスクリーンショットを撮らせるキー（P・Home）: 押した瞬間の取り方と持ち越し（pure）と、Present のラッパを呼ぶ間だけ
// 語を立てる層（unsafe はこの中だけ）。
mod shot_key;
mod shot_key_win;
// ローダーのウィンドウ（status.txt）に出す文の日本語・英語（pure）。
mod shot_win;
mod shots;
mod show_text;
mod sites;
// 観戦のホストの側: ゲームのスレッドが書くリングバッファと、受信スレッドが観戦者へ送る側。
mod spectate_host;
// インターネット越しの観戦（観戦者の側）: ゲーム内のメニューの観戦の「ルームマッチ」を決めた人のゲームが、観戦用のルームへ入って
// 対戦するホストの観戦用のコードを受け、そのホストへ返しを出して JOIN を始めさせる。
mod spectate_join;
// インターネット越しの観戦（配る側）: マッチのホストが観戦用のコードを観戦用のルームへ預けて観戦者を待ち、観戦者の返しを受けて受信スレッドに穴あけを頼む。
mod spectate_offer;
// 観戦を断る配る側: 2 人のどちらかが観戦を許可していないルームマッチのホストが、観戦用のルームで観戦者に断りを返す。
mod spectate_refuse;
mod spectate_ring;
#[cfg(test)]
mod spectate_testkit;
// 観戦者の側: 対戦のホストから受けた試合の記録を組み立てて観戦者のゲームへ入れる判断と、観戦用のコードを読む見張り。
mod spectate_watch;
mod spectate_watch_code;
// 台本のマッチの状態の操作（無敵・ライフ・ゲージ。pure。書き込みの部分は `script_win.rs`）。
mod state_ops;
mod stub;
#[cfg(test)]
mod test_alloc;
// 検査用: 実行できるメモリの偽のコードへ書く `HookEnv`。
#[cfg(test)]
mod test_code;
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
// ゲームが custom.exe の「垂直同期を取らない」を読む 2 か所の答えの差し替え（ini の `[pacing] vsync`。DllMain の中で書くので、unsafe はこの中だけ）。
mod vsync_answer_win;
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
            // ゲームのメニューの保存を呼ぶ 2 か所も、主スレッドが走る前に書く（書けなくても DLL の読み込みは成功のまま）。
            let _ = catch_unwind(AssertUnwindSafe(|| menu_save_win::on_attach(module)));
            TRUE
        }
        Ok(false) | Err(_) => FALSE,
    }
}

#[cfg(test)]
mod dev_assets_guard {
    use std::path::Path;

    /// 資料の有無は判定の関数とは別に、解析のメモの置き場だけで見る（判定が常に偽でも通らないように）。
    #[test]
    fn dev_asset_checks_are_compiled_when_the_dev_files_exist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert_eq!(
            cfg!(dev_assets),
            root.join("analysis").join("notes").is_dir(),
            "cfg dev_assets と資料の有無が食い違う"
        );
    }
}
