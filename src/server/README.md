# src/server

ルームサーバーです。
Cloudflare WorkersとDurable Objectで動く、TypeScriptの小さなサーバーです。
公開しているHiganRToolは、このサーバーをデプロイしたものを使います。

受け持つのは、相手を見つけるまでのことだけです。

- **ルームマッチ**: 同じルーム名を入れた2人の間で、接続コードを1往復だけ受け渡します。渡し終えたらルームを閉じ、その後の通信は2人のPCの間で直接行います。
- **ランダムマッチ**: 待っている人を来た順に2人ずつ組にし、推測できないルーム名を2人へ返します。その後はルームマッチと同じです。
- **観戦の一覧**: 進行中のランダムマッチを観戦者に見せます。
- **アップデート**: ローダーが起動の前に取りに来る署名つきの目録とファイルを、配布している場所（GitHubのReleasesの最新）へ回します。

試合の通信はこのサーバーを通りません。
接続コードをコピーして渡す接続は、サーバーなしで動きます。
エンドポイントと、記録しないものは、リポジトリの直下の`TECHNICAL.md`の「ルームサーバー」にあります。

## 中身

- `src/index.ts`: 入口のWorkerです。パス・バージョン・ルーム名の文字・WebSocketか・繋ぐ回数を見て、それぞれのDurable Objectへ渡します。
- `src/room.ts`: ルーム（1つのルームが1つのDurable Object）。役割（host / guest）・接続コードの受け渡し・期限・相手の切断。
- `src/random_queue.ts`: ランダムマッチの待ち列（ビルド番号ごとに1つ）。
- `src/watch_list.ts`: 観戦の一覧（ビルド番号ごとに1つ）。
- `src/connect_limiter.ts`: IPアドレス（IPv6は/64）ごとに、繋ぐ回数を数えます。前段にCloudflareのRate Limitingも置いています。
- `src/update.ts`: アップデートの目録とファイルの転送。
- `src/rules.ts`: 文字・長さ・期限・閉じる番号・回数の数え方などの決まりと、届いた文の読み方（Workersの機能を使わない純粋な部分）。
- `test/`: 検査です（vitest。Workersの実行環境の中で、本物のWebSocketで叩きます）。
- `wrangler.jsonc`: Workerの設定です（Durable Objectの束縛・回数の制限・アップデートの配布先）。

状態はWebSocketのattachmentと、回数の数えのためのSQLiteの記憶にだけ置いています。

## 動かし方（ローカル）

```
npm --prefix src/server install        # 初回だけ
npm --prefix src/server test           # 検査
npm --prefix src/server run dev        # ローカルで動かす（http://127.0.0.1:8787）
npm --prefix src/server run types      # 編集器の型の手がかり（worker-configuration.d.ts）を作る
```

- `wrangler.jsonc`の`"send_metrics": false`で、wrangler自身の利用状況の送信を切っています。
- `test/fixture.test.ts`は、Rustの側と共有する見本のファイルを読みます。見本はこの公開リポジトリに入っていないので、ここではこの検査だけ動きません。

## 使っているパッケージ

どれも開発の道具（検査とローカルの実行）で、デプロイするWorkerには入りません。
Workerに入るのは`src/`の7ファイルだけで、外のパッケージは使っていません。

- `wrangler` 4.124.0（MIT OR Apache-2.0）: Workersの開発・ローカルの実行・デプロイの道具。
- `vitest` 4.1.11（MIT）: 検査の走らせ役。
- `@cloudflare/vitest-pool-workers` 0.22.0（MIT）: vitestの検査を、Workersの実行環境の中で走らせます。

推移的に入るものの中に、LGPL-3.0-or-later（`@img/sharp-*`）とMPL-2.0（`lightningcss*`）のものがあります。
どちらも開発の道具の中だけで、配布するもの（Worker・DLL・exe）には入りません。
GPLのものはありません。
