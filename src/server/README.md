# src/server —— ルーム名マッチングのサーバー

同じルーム名を入れた 2 人の間で、接続コード（`src/core/src/code.rs`）を 1 往復だけ受け渡すサーバー。
Cloudflare Workers ＋ Durable Object（ルーム 1 つ = Durable Object 1 つ）、TypeScript。待ち方は
WebSocket ＋ Hibernation API。コードを渡し終えたらルームを閉じ、後は P2P。
**サーバーを使うかは任意**で、コピペの接続はサーバー無しで動く。

**まだデプロイしていない。** ここにあるのはローカルで動かして検査するところまで。

## ファイル

| ファイル | 中身 |
|---|---|
| `src/index.ts` | 入口の Worker。パス・バージョン・ルーム名の文字・WebSocket か・1 IP あたりの回数を見て、ルームの Durable Object へ渡す |
| `src/room.ts` | ルームの Durable Object（`Room`）。役割（host / guest）・コードの受け渡し・期限・相手の切断。状態は WebSocket の attachment だけに置く |
| `src/random_queue.ts` | ランダム対戦の待ち列の Durable Object（`RandomQueue`。ビルド番号ごとに 1 つ）。`/v0/random/<ビルド番号>` へ繋いだ人を来た順に 2 人ずつ組にし、組ごとに推測できないルーム名（`q.` ＋ 乱数 120 bit の 16 進 30 文字）を 2 人へ返して閉じる。その後は今のルーム（`room.ts`）へそのルーム名で入る。待ち列の期限は 10 分、割り当てのルームの期限は 1 分。状態は WebSocket の attachment だけ |
| `test/random.test.ts` | ランダム対戦の検査（組・先後・3 人以上・抜ける・組にした直後に片方が消える・期限・ビルド番号・入口の断り・回数の制限・割り当てのルーム名の形と推測できなさ） |
| `src/watch_list.ts` | 進行中のランダム対戦の一覧の Durable Object（`WatchList`。ビルド番号ごとに 1 つ）。載せるエンドポイント `/v0/watch/<ビルド番号>/post`（対戦のホストが割り当てのルーム名 `q.…`・名前・同期方式・観戦者の数と上限を送る。サーバーが観戦用のルーム名 `w.…` を導き、`q.…` は一覧に出さない。1 IP から 2 件・全体で 100 件・切れたら外す）と、見るエンドポイント `/v0/watch/<ビルド番号>/view`（観戦者が開いている間だけ。開いたときと変わるたびに全体を送る。変化は 1 秒に 1 回にまとめる）。状態は WebSocket の attachment だけ |
| `test/watch.test.ts` | 一覧の検査（載る・q 以外を断る・w 名の導き方・名前と数の範囲・件数の上限・切れたら消える・見るエンドポイントに変化が届く・ビルド番号ごと・期限・入口の断りと回数の数え・既存の口が変わらないこと） |
| `src/connect_limiter.ts` | 繋ぐ回数を数える Durable Object（`ConnectLimiter`。IP〔IPv6 は /64〕ごとに 1 つ。60 秒の滑る期間に 30 回。前段に Rate Limiting の束縛〔60 秒に 120 回〕。許した時刻だけを SQLite の記憶に持つ） |
| `src/update.ts` | 更新のエンドポイント（`/update/manifest` と `/update/files/<名前>`）。配るバージョンのローダーが花映塚を起こす前に取りに来る署名つきの目録とファイルを、いまの配り場所（`wrangler.jsonc` の `vars.UPDATE_BASE_URL`。GitHub の Releases の最新）へ 302 で回すだけ。名前は規則（`isUpdatableFileName`。`src/core/src/update.rs` の `is_allowed_name` と同じ規則で、標本の表を両方の検査に持ち `tools/make_dist.py` が突き合わせる）。回数の数え・Durable Object には触らない |
| `test/update.test.ts` | `update.ts` の検査（回す先・表に無い名前の 404・GET/HEAD だけ・設定が使えないときの 503・ルームのエンドポイントが変わらないこと） |
| `src/rules.ts` | 決まり（文字・長さ・期限・閉じる番号・回数の数え方と鍵）と、届いた文の読み方。Workers の API を使わない純粋な部分 |
| `test/connect_limit.test.ts` | 回数の制限の検査（数え方・鍵の純粋な検査と、本物の Durable Object での組み上がり・退去・アラーム・fail-open） |
| `test/rules.test.ts` | `rules.ts` の単体の検査 |
| `test/room.test.ts` | 入口 → ルームを、ローカルの workerd の上で本物の WebSocket で叩く組み上がりの検査 |
| `test/helpers.ts` | 検査のヘルパー（繋ぐ・文を順に待つ）。送り元は文書用の IPv6（`2001:db8::/32`）、コードは合成 |
| `wrangler.jsonc` | Worker の設定（Durable Object の束縛〔ルーム・回数の数え役・ランダム対戦の待ち列・ランダム対戦の一覧〕・回数の前段の束縛〔ratelimits〕・SQLite の記憶・利用状況の送信を切る） |
| `vitest.config.ts` | 検査を Workers の実行環境（miniflare）で走らせる設定 |
| `tsconfig.json` | 編集器の型の手がかり用（検査と wrangler は型を剥がして走らせるので、建て方には効かない） |

## 動かし方（ローカルだけ。外へは繋がない）

```
npm --prefix src/server install        # 初回だけ（npm の registry から開発の道具を取る）
npm --prefix src/server test           # 検査（vitest ＋ @cloudflare/vitest-pool-workers）
npm --prefix src/server run dev        # wrangler dev（http://127.0.0.1:8787 。ws://.../v0/rooms/<ルーム名>）
npm --prefix src/server run types      # worker-configuration.d.ts を作る（編集器の型用。git に入れない）
```

- `wrangler.jsonc` の `"send_metrics": false` で、wrangler 自身の利用状況の送信（Cloudflare への接続）を
  切っている。念のため環境変数 `DO_NOT_TRACK=1` も付けて走らせると確実。
- `npm install` は `esbuild` と `workerd` の postinstall を「許可待ち」として飛ばす（npm 11 の既定）。
  飛ばしたままでも検査も `wrangler dev` も動く。
- デプロイ（`wrangler deploy`）とログイン（`wrangler login`）は Cloudflare のアカウントを作った後。

## パッケージ

どれも開発の道具（検査とローカルの実行）で、デプロイする Worker の中身には入らない
（Worker に入るのは `src/` の 6 ファイルだけ。外のパッケージを import していない）。

| パッケージ | バージョン | 用途 | ライセンス |
|---|---|---|---|
| `wrangler` | 4.124.0 | Workers の開発・ローカル実行（`wrangler dev`）・型の生成・デプロイの道具 | MIT OR Apache-2.0 |
| `vitest` | 4.1.11 | 検査の走らせ役 | MIT |
| `@cloudflare/vitest-pool-workers` | 0.22.0 | vitest の検査を Workers の実行環境（miniflare / workerd）の中で走らせる | MIT |

バージョンは固定した（`@cloudflare/vitest-pool-workers` 0.22.0 が vitest `^4.1.0` と wrangler 4.124.0 を要るため。
vitest の最新は 5.0.2 だが、この組み合わせは 4 系）。

推移的に入るもの（`package-lock.json` の 159 項）のライセンスは MIT・Apache-2.0・ISC・BSD・0BSD・CC0 と、
`@img/sharp-*`（画像の束縛の模型。libvips の LGPL-3.0-or-later）と `lightningcss*`（vite の CSS。MPL-2.0）。
**GPL は無い**（`package-lock.json` の `license` を数えた）。LGPL と MPL のものも開発の道具の
中だけで、配る物（Worker・dll・exe）には入らない。
