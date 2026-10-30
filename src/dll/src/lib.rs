//! HiganRTool.dll —— th09.exeに注入されるDLLの入口。
//!
//! 注入されたら別スレッドで自分のexeの同定と、地点の表（`th09_addr.rs`）の各地点の期待バイト列の照合（読むだけ）をする。
//! **全部が通ったときだけ** `0x0043112D`のcallのrel32の4バイトを自前の入口（`stub.rs`）へ向ける。
//! 入口は回数を数え、tickごとの記録をDLLの中のリングバッファに書いて元の飛び先へ飛ぶ（素のアセンブリ。ゲームのスレッドでRustのコードは走らない）。
//! 設置の後は読み取りのスレッドがリングバッファを読み、`_ticks.csv`などへ追記してログに書く。
//! ログとほかのファイルはDLLと同じフォルダの`logs\`に1回ごとに別名で書く。
//!
//! DllMainはローダーロックの下で走る。そのためミューテックスを取ってスレッドを作るだけで待たない。
//! フックの設置もネットワークもウィンドウの作成もしない。
//! 例外はパッドの十字キーの入口（`pad_extra_win::on_attach`）などで、ここでrel32を書く。
//! ローダーはゲームを`CREATE_SUSPENDED`で作り、`LoadLibraryW`が戻るまで主スレッドを走らせない。
//! そのためゲームのコードを誰も実行していない唯一の時点で書ける。
//! unsafeは`*_win.rs`・`stub.rs`・`draw.rs`などWin32とアセンブリの層に閉じ込め、他のモジュールは`#![forbid(unsafe_code)]`。

// ネットワークアダプターの一覧（`bind = auto`が経路を取得できないとき・LANのホストが自分のアドレスを見せるとき。unsafeはこの中だけ）。
mod adapters_win;
mod alloc_count;
mod alloc_win;
// 起動直後のリフレッシュレートの計測のBeginSceneの呼び出しの差し替え（デバイスが失われていたら計測を打ち切る。DllMainの中で書くのでunsafeはこの中だけ）。
mod calib_guard_win;
// ゲームの中のメニューが接続コードをコピーする・貼る部分（本物のクリップボードのunsafeは`clipboard_win.rs`の中だけ）。
mod clipboard;
mod clipboard_win;
mod copies;
mod crash_win;
// 読み込まれているd3d8.dllの場所とバージョン情報（本ログの1行。unsafeはこの中だけ）。
mod d3d8_info_win;
mod detour;
// 数字キー（上段の0〜9）で遅れの量を替える入口。押した瞬間の取り方（pure）とキーの読み（`GetAsyncKeyState`。unsafeはこの中だけ）。
mod digit_keys;
mod digit_keys_win;
mod draw;
// マッチ終了時のメニューの項目の1P・2Pの縁取り（描き方は`higanr_core::end_outline`。unsafeはこの中だけ）。
mod end_outline_win;
mod feeds;
mod hook;
mod hook_win;
// LAN対戦の申し込みの運び手（stdのソケットだけ。unsafe無し）。
mod lan_link;
// 接続ごとの本ログの行（接続した行・stallの内訳・RTT・観戦者の出入り。pure）。
mod link_log;
mod menu;
// ゲームのメニューから保存するリプレイ（長い試合で落ちる不具合の避け方とHiganRToolの印）。
// 判断は`menu_save.rs`（pure）。入口と設置と印を足すスレッドは`menu_save_win.rs`（unsafeはこの中だけ）。
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
// ルームサーバーとの送受信を受け持つ部分。unsafeは`room_win.rs`（WinHTTPのWebSocket）の中だけ。監視の処理（`net_code_game`）から対戦の「ルームマッチ」で使う。
// 製品のビルドで使わない関数が残るのでdead_codeを許す。
#[cfg_attr(not(test), allow(dead_code))]
mod net_room;
mod net_select;
// ゲームの中のメニューの、ゲームのスレッドの側と描く行の盤。
mod net_udp;
mod net_ui;
mod net_watch;
mod net_win;
mod netplay;
// 接続した知らせ（音と点滅。unsafeはこの中だけ）。
mod notify_win;
mod observe;
// 文字の描画（unsafeは`overlay_win.rs`: D3D8のvtableの呼び出し・GDI・文字の表の置き場）。
mod overlay;
mod overlay_win;
// ペースBと時刻のフック。unsafeは`pacing_win.rs`（QPC・入口のアセンブリ・待機タイマー）にもある。
mod pacing;
mod pacing_win;
// パッドの十字キー（POVハット）を方向に足す入口。判断と設置の手順は`pad_extra.rs`（pure）。
// 本物の読み・ゲームが呼ぶ入口・`DllMain`からの設置は`pad_extra_win.rs`（unsafeはこの中だけ）。
mod log_mode;
// メインループのTestCooperativeLevelの差し替え（デバイスが失われている間は10 msずつ寝る。DllMainの中で書くのでunsafeはこの中だけ）。
mod lost_sleep_win;
mod main_log;
mod match_events;
mod pad_extra;
mod pad_extra_win;
mod probe;
mod record_win;
mod render_only;
// 自分のPCのVPNのネットワークのスナップショット（LANのアドレスの入力が使う。unsafe無し）。
mod vpn_snap;
// 自動保存（unsafeは`replay_save_win.rs`の中だけ）。
mod replay_save;
mod replay_save_win;
// 素の花映塚でのリプレイの再生の監視の処理（判断は`replay_watch.rs`（pure）。ゲームのスレッドと本ログへの受け渡しは`replay_watch_win.rs`）。
mod replay_watch;
mod replay_watch_win;
// 描画用のVMのフィールドの診断と除外（pure）。
mod rollback_draw;
// rollbackのモードの製品の経路の部品（pure。Win32とゲームのメモリは`rollback_play_win.rs`）。
// `netplay/rollback.rs`がステートマシンを進める。`script_win.rs`が走査のラッパのフック・入力の地点・Presentの後から呼ぶ。
mod rollback_play;
mod rollback_play_win;
mod rollback_probe;
#[cfg(test)]
mod rollback_testkit;
// 進め直しを1回の描画の中で行う形（pure。入口のアセンブリは`rollback_win.rs`）。
mod rollback_resim;
mod rollback_win;
// ルームサーバーのWebSocket（WinHTTP。unsafeはこの中だけ）。`net_room`から使う（製品で使わない関数が残るのでdead_codeを許す）。
#[cfg_attr(not(test), allow(dead_code))]
mod room_win;
// 利用者へ配布するバージョンでiniが無いときは、配布するiniと同じ中身をDLLのフォルダへ書き出す（起動時に1回）。
mod ini_seed;
// メニューで決定した設定をiniの該当の1行へ書き戻す（別のスレッド）と、以前のバージョンの別ファイルの引き継ぎ（pure。unsafe無し）。
mod ini_save;
mod script_watch;
mod script_win;
mod scripted;
mod session;
// 自分のPCにだけゲーム自身のスクリーンショットを撮らせるキー（P・Home）。
// 押した瞬間の取り方と持ち越し（pure）と、Presentのラッパを呼ぶ間だけワードを立てる層（unsafeはこの中だけ）。
mod shot_key;
mod shot_key_win;
// ローダーのウィンドウ（status.txt）に出す文の日本語・英語（pure）。
mod shot_win;
mod shots;
mod show_text;
mod sites;
// 観戦のホストの側。ゲームのスレッドが書くリングバッファと、受信スレッドが観戦者へ送る側。
mod spectate_host;
// インターネット越しの観戦（観戦者の側）。ゲーム内のメニューの観戦の「ルームマッチ」を選択した人のゲームが観戦用のルームへ入る。
// 入ったら対戦するホストの観戦用のコードを受け、そのホストへ返答を出してJOINを始めさせる。
mod spectate_join;
// インターネット越しの観戦（配信する側）。マッチのホストが観戦用のコードを観戦用のルームへ登録して観戦者を待つ。観戦者の返答を受けたら受信スレッドに穴あけを頼む。
mod spectate_offer;
// 観戦を断る配信側。2人のどちらかが観戦を許可していないルームマッチのホストが観戦用のルームで観戦者に断りを返す。
mod spectate_refuse;
mod spectate_ring;
#[cfg(test)]
mod spectate_testkit;
// 観戦者の側。対戦のホストから受けた試合の記録を組み立てて観戦者のゲームへ渡す判断と、観戦用のコードを読む監視の処理。
mod spectate_watch;
mod spectate_watch_code;
// 台本のマッチの状態の操作（無敵・ライフ・ゲージ。pure。書き込みの部分は`script_win.rs`）。
mod state_ops;
mod stub;
#[cfg(test)]
mod test_alloc;
// 検査用の`HookEnv`。実行できるメモリの偽のコードへ書く。
#[cfg(test)]
mod test_code;
// キーボード2台を分ける入力の受け取り（Raw Inputの登録と台ごとの押下の表。unsafeはこの中だけ）。
mod split_kb_win;
// ルーム名の入力がウィンドウから文字を受ける仕組み（ウィンドウの手続きの差し替えとIME。unsafeはこの中だけ）。
mod text_input_win;
mod th09_addr;
mod ticks;
// 画面の言語（日本語・英語）の今の値とiniへの書き戻し（pure）。OSのUIの言語の読みは`ui_lang_win.rs`（unsafeはこの中だけ）。
mod ui_lang;
mod ui_lang_win;
// 試験のまとめ（本ログの最後の節。pure）。
mod trial_summary;
// 遊んでいる間の新しいバージョンの確認（専用のスレッドが署名つきの目録を取得する。判断は`higanr_core::update_notice`）。
mod update_check;
// 家のルーターへUPnPでポートの割り当てを頼む（通信はLANの中だけ。SSDPを送るインターフェースの指定だけ`net_win.rs`のunsafe）。
mod upnp;
// ゲームがcustom.exeの「垂直同期を取らない」を読む2か所の答えの差し替え（iniの`[pacing] vsync`。DllMainの中で書くのでunsafeはこの中だけ）。
mod vsync_answer_win;
mod watch_feed;
mod win;
// ウィンドウモードのウィンドウを作る呼び出しの差し替え（iniの`[window]`。DllMainの中で書くのでunsafeはこの中だけ）。
mod window_win;

use core::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

use windows_sys::Win32::Foundation::HINSTANCE;
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::core::BOOL;

const TRUE: BOOL = 1;
const FALSE: BOOL = 0;

/// DLLの入口。OSのローダーだけが呼ぶ。`extern "system"`は32bitのWindowsではstdcall。
/// パニックは`catch_unwind`で受けて外へ出さず、受けたらFALSEを返す。
/// ローダーは`LoadLibraryW`の戻り値が0になったと見て、ゲームを始めずに終える。
#[unsafe(no_mangle)]
#[allow(non_snake_case)] // 名前はWin32が決めたもの
pub extern "system" fn DllMain(module: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason != DLL_PROCESS_ATTACH {
        return TRUE;
    }
    match catch_unwind(AssertUnwindSafe(|| win::on_process_attach(module))) {
        Ok(true) => {
            // 十字キーの入口はゲームの主スレッドがまだ走っていない今のうちに書く。
            // 失敗しても読み取りのスレッドは起きているのでDLLの読み込みは成功のまま返す（パッドの十字キーが効かないだけ）。
            let _ = catch_unwind(AssertUnwindSafe(|| pad_extra_win::on_attach(module)));
            // ゲームのメニューの保存を呼ぶ2か所も主スレッドが走る前に書く（書けなくてもDLLの読み込みは成功のまま）。
            let _ = catch_unwind(AssertUnwindSafe(|| menu_save_win::on_attach(module)));
            // マッチ終了時のメニューの項目を描く1か所も同じ（書けなければ縁取りが出ないだけ）。
            let _ = catch_unwind(AssertUnwindSafe(end_outline_win::on_attach));
            TRUE
        }
        Ok(false) | Err(_) => FALSE,
    }
}

#[cfg(test)]
mod dev_assets_guard {
    use std::path::Path;

    /// 資料の有無は判定の関数とは別に解析のメモの置き場だけで見る（判定が常に偽でも通らないように）。
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
