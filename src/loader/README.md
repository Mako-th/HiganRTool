# src/loader

`HiganRTool.exe`（クレート `higanrtool_loader`。`build.bat` が `build/bin/HiganRTool.exe` へコピーする）。
th09.exe をサスペンド状態で起動し、`HiganRTool.dll`（`src/dll/`）を注入してから再開する。

## 起動前の更新（配るバージョンだけ。feature `dist`）

`update.rs`（流れ・置き換え・戻し。偽の HTTP サーバーと偽のウィンドウで検査）と `http_win.rs`（WinHTTP の GET。unsafe はここ）。
花映塚を起こす前に、ルームサーバーのアドレス（`higanr_core::update::UPDATE_SERVER` ＋ `MANIFEST_PATH`）から署名つきの目録を取り、
今より新しいバージョンなら「はい / いいえ」のウィンドウで尋ね、「はい」なら必要なファイルを `.old` に名前を変えて置き換え、同じ引数で
新しい exe を起こして終わる。失敗は元へ戻す。待ちは全体で約 2 秒。開発用のビルド・実機の回（`--ini` などの引数）・自己検査・
ini の `[loader] update_check = 0` では確かめない。目録の読み・署名・許可リストは `src/core/src/update.rs`、署名の道具は `src/manifest_sign`。
（`update` や `setup` を名前に含む exe は、Windows が「インストーラー」とみなして管理者の権限を求める（os error 740）ことがあるので、
道具の exe の名前には使わない。`manifest_sign` はそのため。）
