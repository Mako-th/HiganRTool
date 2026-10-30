# src/http_win

クレート`higanr_http_win`。WinHTTPでGETを1本する部品です。
`src/loader`のHiganRTool.exe（起動前の更新の目録とファイルの取得）と`src/dll`のHiganRTool.dll（遊んでいる間の新しいバージョンの知らせ）の両方が使います（`unsafe`が要るので`src/core`には置けません）。

- OSのTLSとプロキシの設定をそのまま使います。クレートは足しません。
- 302はWinHTTPが辿ります（httpsからhttpへの転送は断ります）。本文は`max`バイトまでで、`Content-Length`と受けた長さが違えば`Truncated`を返します。
- 待ちの上限は`Timeouts`の4つ（操作ごと）です。全体の上限は呼ぶ側が決めます。
- 目録の署名の検証と読みは`higanr_core::update`が行い、ここは通信だけを持ちます。
