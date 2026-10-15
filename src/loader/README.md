# src/loader

起動用のexeです（クレート`higanrtool_loader`。成果物は`higanrtool_loader.exe`で、配布するときは`HiganRTool.exe`に名前を変えます）。
花映塚（th09.exe）を止めた状態で起動し、`HiganRTool.dll`（`src/dll`）を読み込ませてから動かします。

- OSのキーボード・マウスの入力を送ったり、前面のウィンドウを奪ったりする呼び出しはしません。
- ゲームのフォルダに書くのは、ツールをその中に置いたときの`HiganRTool.ini`だけです。

仕組みの全体は、リポジトリの直下の`TECHNICAL.md`の「起動と注入」と「自動アップデート」にあります。

## 中身

- **起動と注入**: `launch.rs`（サスペンドで起動 → DLLのパスを書き込む → `LoadLibraryW`のスレッド → 再開の手順と、失敗したときの後始末）・`staged.rs`・`real.rs`（本物のWin32）・`win.rs`（Win32の呼び出しをまとめたトレイト。検査では偽物に差し替えます）・`ledger.rs`（開いたハンドルと確保した領域を数え、閉じ忘れを検査で見つけます）・`env_block.rs`・`imports.rs`（DLLが使う関数がこのWindowsにあるかを先に確かめます）・`selftest.rs`（`--selftest`）・`synth.rs`（検査用の合成のPE）。
- **花映塚の場所**: `game_pick.rs`（初回だけ、th09.exeのあるフォルダを選んでもらって覚えます）・`folder_dialog.rs`（Windows標準のフォルダ選択）。
- **自動アップデート**（配布するビルドだけ。機能`dist`）: `update.rs`（流れ・置き換え・失敗したときの戻し・ダウンロードの進み具合）・`http_win.rs`（WinHTTPのGET）・`post_update.rs`（更新して起動し直した初回だけ、`HiganRTool.ini`を配る形に合わせて設定の値を引き継ぎ、`release_notes/`の変更点を出してEnterを待ちます）。目録の読みと署名の確かめは`higanr_core::update`です。
- **接続コード**: `code_relay.rs`（ゲームの外で接続コードを受け渡すときのウィンドウ）。
- **起動の前の確かめとログ**: `writable.rs`（ツールのフォルダに書き込めるか）・`freshness.rs`（開発用のビルドで、古い成果物での起動を断る）・`crash_log.rs`（ローダーのログと、落ちたときの記録）・`hold.rs`（断って終わるときに、ウィンドウをEnterまで残すか）・`ui_lang.rs`・`cli.rs`。

`unsafe`を書いてよいのは`real.rs`・`folder_dialog.rs`・`http_win.rs`だけで、ほかのモジュールは`#[forbid(unsafe_code)]`です。

## 自動アップデート

配布するビルドだけが、花映塚を起動する前に、ルームサーバーから署名つきの目録を取ります。
今より新しいバージョンがあれば「はい / いいえ」で尋ね、「はい」なら目録に載ったファイルを置き換えて、同じ引数で起動し直します。
置き換えてよいファイルの名前は`higanr_core::update`の`SHIPPED_FILES`で決めています。
途中で失敗したら元に戻します。
確かめる待ちは全体で約2秒までで、届かなければそのまま起動します。
確かめるのは配布するビルドの通常の起動だけで、開発用のビルド・開発用の引数を付けた起動・`--selftest`・`HiganRTool.ini`の`[loader] update_check = 0`では確かめません。
置き換えたファイルの古いものは`.old`として残り、新しいバージョンで花映塚を起動できた次の起動で消します。

目録に署名する道具は`src/manifest_sign`です。
道具のexeの名前には`update`や`setup`を使いません（Windowsがインストーラーとみなして、管理者の権限を求めることがあるためです）。

## ビルド

```
cargo build --release -p higanrtool_loader
```

32bitの`i686-pc-windows-msvc`向けです（`.cargo/config.toml`で指定しています）。

## 使っているパッケージ

- `windows-sys`（MIT OR Apache-2.0）: Win32の宣言だけです。開けている機能と、それぞれを何に使うかは`Cargo.toml`に書いています。
