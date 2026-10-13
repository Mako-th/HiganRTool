# src/dll

th09.exe に注入される DLL（クレート `higanrtool_dll`。`build.bat` が `build/bin/HiganRTool.dll` へコピーする）。
注入されたら別スレッドで自分の exe の同定と地点の表の照合（読むだけ）をし、全部が通ったときだけフックを書く。全体の流れは `src/lib.rs` の doc。
ゲームに触らない純粋な処理（プロトコル・フレームの式・リプレイの解析）は `src/core/`。unsafe はこのクレートの、フック・ゲームのメモリ・D3D8 の層に閉じ込める。

## 計測と設置

- `src/lib.rs` — `DllMain`（二重注入のミューテックスを取り、読み取りのスレッドを起こすだけ）
- `src/th09_addr.rs` — アドレスと期待バイト列・禁止の範囲の定数、書く行の名前（ここ 1 か所だけ）
- `src/sites.rs` — 地点の表の型と表の検査（`higanr_core::patch` を呼ぶ）
- `src/probe.rs` — 照合の手順とログの文（読むだけ。Win32 は `Env` の向こう）
- `src/session.rs` — スレッドの 1 回の実行: 照合 → 設置の条件 → 設置 → 計測
- `src/hook.rs` — コードの書き換えの手順（止める・EIP を全長で見る・読み直し・書く・再開。Win32 は `HookEnv` の向こう）
- `src/detour.rs` — inline detour の計画と設置（`E9 rel32` ＋ トランポリン。Win32 は `DetourEnv` の向こう。pure）
- `src/draw.rs` — 描画の地点の素のアセンブリの入口と描画のリングバッファ（1 件 16 バイト。unsafe はアセンブリだけ）
- `src/stub.rs` — 入力と待ちの差し替え先の入口（素のアセンブリ）と、入口が触る静的変数（unsafe あり）
- `src/render_only.rs` — 描画だけのフレームの予定と集計・`_draws.csv` / `_renderonly.csv`（pure）
- `src/observe.rs` — 設置の後の計測（100 ms ごとに tick と描画のリングバッファを読み、1 秒ごとに追記、描画だけのフレームの予定の駆動、D3D8 の下見）
- `src/ticks.rs` — リングバッファの 1 件の形（先頭とコピーの置き場・コピーの表 `COPY_TABLE`・描画だけのフレームの印 `+52`）・集計・CSV の先頭の列（pure）
- `src/copies.rs` — コピーの表の確かめ、コピーの列・差分・`_snap.bin` の先頭（pure）
- `src/menu.rs` — メニューとライフの読みの判定・`_menu.csv` の行・刻みの集計（pure）
- `src/names.rs` — ログと隣のファイルの名前・ミューテックスの名前・「ゲームのフォルダの中か」の判定
- `src/test_alloc.rs` — 検査のときだけの割り当て器（止めている間にヒープを使っていないかを数える）

## Win32 の層（unsafe あり）

- `src/win.rs` — 読む側
- `src/hook_win.rs` — スレッドの停止・自分のプロセスのコードへの書き込み・トランポリンの頁の確保（`HookEnv` と `DetourEnv` の本物）
- `src/menu_win.rs` — メニューとライフの読み取りのスレッド（`ReadProcessMemory` と高分解能の待機タイマー）
- `src/record_win.rs` — TSC・QPC・CPUID・ゲームの時計の分母の読み取り・隣のファイル・メニューのスレッドの起こしと止め

## ネットワーク

- `src/net_ring.rs` — 受信のリング（受信スレッドが書き、ゲームスレッドが取り出す。単一書き手・単一読み手、64 枠、原子変数だけ。満杯と大きすぎるものは捨てて数える）
- `src/net_win.rs` — 受信のイベントと、高分解能の待機タイマーとイベントを待つ `Waiter`（unsafe あり）
- `src/net_udp.rs` — 通信層: ガード（ネットワークの範囲 `scope` の外は開く前に断る。既定は loopback）・UDP のソケット・受信スレッド・ゲームスレッドのインターフェース `GameNet`
- `src/netplay.rs` — 入力の地点の判断（pure）: 繋がりのセッション（`higanr_core::link`）を持ち、tick ごとに Local / 0 を feed / lockstep のフレームを feed / 描画だけのフレームを決める。ゲームは `Game`、ネットワークは `Net` の向こう
- `src/netplay/` — 検査だけ: 偽のゲームと仮の時計のネットワーク、偽のゲーム 2 つを繋ぐ pure な検査、偽のゲーム＋本物の通信層と peer_sim を loopback で繋ぐ組み上がりの検査（`#[ignore]`。全件の回で走る）
- `src/net_game.rs` — ゲームの窓の本物（pure。メモリと関数は `NetIo` の向こう）。判断を入口の戻り値にする `entry_return`
- `src/net_config.rs` — `HiganRTool.ini` の `[netplay]` を読む（pure。知らないキーとネットワークの範囲の外の宛先を断る）
- `src/net_watch.rs` — ゲームのスレッドから読み取りのスレッドへ: 使ったフレームのリングバッファ（`_net_frames.csv`）とネットワークの数（pure。原子変数だけ）
- 入口の側: `src/stub.rs` の `script_on_tick` の戻り値 2（描画だけのフレーム）と開始のフックの入口 `match_start_entry`、`src/script_win.rs` のネットワークのセッション・`net_tick`・`match_start_on`、`src/session.rs` の ini を読む → 開始のフック → セッションを置く → 動かす順
