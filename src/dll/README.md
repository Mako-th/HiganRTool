# src/dll

花映塚（th09.exe）に読み込ませるDLLです（クレート`higanrtool_dll`。成果物は`higanrtool.dll`で、配布するときは`HiganRTool.dll`に名前を変えます）。
花映塚のプロセスの中で動く本体で、フック・ゲームのメモリの読み書き・文字の描画・UDPの送受信・ルームサーバーとの通信を受け持ちます。
ゲームに触らない純粋な処理（プロトコル・同期の状態機械・リプレイの解析）は`src/core`にあります。

仕組みの全体は、リポジトリの直下の`TECHNICAL.md`にあります。
各モジュールの細かい決まりは、そのファイルの先頭のコメント（`//!`）に書いています。

## 読み込まれてからの流れ

1. **`DllMain`**（`src/lib.rs`）: ローダーロックの下で走るので、二重に読み込まれていないかをミューテックスで確かめ、作業用のスレッドを起こすだけで、待ちません。
   例外はパッドの十字キーの入口（`pad_extra_win.rs`）で、ゲームの主スレッドがまだ1命令も走っていないこの時点で書きます。
2. **照合**（`session.rs`・`probe.rs`）: 作業用のスレッドが、自分のexeが花映塚1.50aかを確かめ、フック地点の表（`th09_addr.rs`）のすべての地点の期待バイト列を照合します（読むだけです）。
3. **設置**（`hook.rs`・`detour.rs`・`hook_win.rs`）: すべてが合ったときだけ、ゲームのスレッドを止めてフックを書きます。
   1地点でも合わなければ、1バイトも書かずにやめて、理由をログに書きます。
4. **動作**: 入力の地点のフック（`stub.rs`）が1フレームに1回呼ばれ、ネットワーク対戦・観戦・ゲーム内のメニューの判断（`netplay.rs`・`net_ui.rs`）を進めます。
   設定は`HiganRTool.ini`から読みます（`net_config.rs`）。

## 中身

役割ごとの、主なファイルです。
名前が`_win.rs`で終わるファイルが、Win32・ゲームのメモリ・D3D8に触る層です。
`unsafe`はその層と、素のアセンブリの入口（`stub.rs`・`draw.rs`）に閉じ込め、1か所ずつ`// SAFETY:`で理由を書いています。
ほかのモジュールは`#![forbid(unsafe_code)]`です。

- **照合と設置**: `th09_addr.rs`（花映塚のアドレスと期待バイト列。この値はここの1か所にだけ置きます）・`sites.rs`・`probe.rs`・`session.rs`・`hook.rs`・`detour.rs`・`hook_win.rs`・`stub.rs`・`win.rs`。
- **ネットワーク**: `net_udp.rs`（UDPのソケット・受信スレッド・接続してよいアドレスの範囲）・`net_ring.rs`（受信スレッドからゲームのスレッドへの受け渡し）・`net_win.rs`（相手を待つ待ち）・`upnp.rs`・`adapters_win.rs`・`lan_link.rs`・`vpn_snap.rs`（自分のPCのVPNのネットワークの控え）。
- **対戦**: `netplay.rs`と`netplay/`（入力の地点の判断。delay方式とrollback方式・相手を待つ間の切断）・`net_game.rs`（ゲームのメモリの読み書き）・`net_select.rs`と`net_portrait.rs`（キャラ選択などの画面の同期）・`rollback_play.rs`と`rollback_play_win.rs`と`rollback_resim.rs`と`rollback_draw.rs`（rollbackのための状態の保存と進め直し）・`digit_keys.rs`と`digit_keys_win.rs`（Ctrl＋数字などのキー）・`shot_key.rs`と`shot_key_win.rs`（ネット対戦の間も自分のPCにだけ撮るスクリーンショット）・`match_events.rs`。
- **接続コードとルームサーバー**: `net_code.rs`・`net_code_game.rs`・`clipboard.rs`と`clipboard_win.rs`・`net_room.rs`と`room_win.rs`（WinHTTPのWebSocket）・`watch_feed.rs`（観戦の一覧）。
- **観戦**: `spectate_host.rs`と`spectate_ring.rs`と`spectate_offer.rs`（配る側）・`spectate_watch.rs`と`spectate_join.rs`と`spectate_watch_code.rs`（観戦者の側）。
- **ゲーム内のメニューと描画**: `net_ui.rs`（メニューのゲームのスレッドの側。状態機械は`higanr_core::net_menu`）・`overlay.rs`と`overlay_win.rs`（文字の描画）・`draw.rs`・`text_input_win.rs`（ルーム名の入力）・`ui_lang.rs`と`ui_lang_win.rs`・`notify_win.rs`（接続したときの音とタスクバーの点滅。前面は奪いません）・`window_win.rs`（ウィンドウの大きさ）・`pad_extra.rs`と`pad_extra_win.rs`（パッドの十字キー）。
- **リプレイ**: `replay_save.rs`と`replay_save_win.rs`（自動保存と印）・`replay_watch.rs`と`replay_watch_win.rs`（素の花映塚での再生を見張り、照合のための1行をログに書く）。
- **ペース**: `pacing.rs`と`pacing_win.rs`（フレームのペース）。
- **設定とログ**: `net_config.rs`（`HiganRTool.ini`。誤りのある行は既定値で読みます）・`ini_seed.rs`・`ini_save.rs`・`main_log.rs`・`log_mode.rs`・`names.rs`・`observe.rs`・`crash_win.rs`（落ちたときの記録）・`show_text.rs`（ローダーのウィンドウに出す文）。
- **開発用の計測と検査**: `script_win.rs`と`scripted.rs`と`script_watch.rs`（開発用の台本で入力を流す）・`state_ops.rs`・`menu.rs`と`menu_win.rs`・`ticks.rs`・`copies.rs`・`feeds.rs`・`render_only.rs`・`shots.rs`と`shot_win.rs`・`alloc_count.rs`と`alloc_win.rs`・`rollback_probe.rs`と`rollback_win.rs`・`trial_summary.rs`・`test_alloc.rs`・`rollback_testkit.rs`・`spectate_testkit.rs`。

## ビルド

```
cargo build --release -p higanrtool_dll
```

32bitの`i686-pc-windows-msvc`向けです（`.cargo/config.toml`で指定しています）。
64bitのDLLは花映塚に読み込めません。

- 機能`dist`は、配布するzipのビルドだけが開けます。フック地点の表の出典の文字列を成果物に入れず、配る設定ファイルの中身を持ちます。
- 検査の一部は、この公開リポジトリに入っていない開発用のデータを読むので、ここではビルドできません（`TECHNICAL.md`の「ビルドと検査」）。

## 使っているパッケージ

- `windows-sys`（MIT OR Apache-2.0）: Win32の宣言だけです。開けている機能と、それぞれを何に使うかは`Cargo.toml`に書いています。

TLS・WebSocket・HTTPは、Windowsの機能（WinHTTP）をそのまま使います。
