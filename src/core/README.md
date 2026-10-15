# src/core

HiganRToolの中核です（クレート`higanr_core`）。
Win32にもゲームのメモリにも触らない、純粋な処理だけを置いています。
DLL（`src/dll`）・ローダー（`src/loader`）・偽の対戦相手（`src/peer_sim`）・リプレイの修正の道具（`src/replay_fix`）が、同じものを使います。

- `#![forbid(unsafe_code)]`で`unsafe`を禁止しています。
- ネットワークから来るバイト列は、すべてここで読みます。
- ゲームのスレッドで呼ぶ部分は、ヒープを確保せず、パニックしないように書いています（`tests/alloc_check.rs`で確保の回数を数えています）。

仕組みの全体は、リポジトリの直下の`TECHNICAL.md`にあります。
各モジュールの細かい決まりは、そのファイルの先頭のコメント（`//!`）に書いています。

## 中身

役割ごとの、主なファイルです。

- **プロトコル**: `wire.rs`（パケットのエンコードとデコード。プロトコルのバージョン`PROTO_VER`もここ）・`auth.rs`（認証タグ）・`seal.rs`（使い捨ての鍵の交換と暗号化）・`handshake.rs`（接続の手順）・`link.rs`（受けたパケットをどの状態機械へ渡すか。DLLと`peer_sim`が共有）・`liveness.rs`（PINGと、沈黙による切断）・`pong.rs`。
- **同期**: `frame.rs`（フレーム番号の式）・`lockstep.rs`（delay方式）・`rollback.rs`（rollback方式）・`word_ring.rs`（入力の置き場）・`check.rs`（同期の確かめ）・`selection.rs`と`menu_model.rs`（キャラ選択などの画面の同期）・`match_gate.rs`（対戦の成立）・`delay_pick.rs`と`mode_pick.rs`（ディレイと同期方式の選び方）・`seat_pick.rs`（席）・`testnet.rs`（検査用の2台の模型）。
- **接続**: `code.rs`と`code_files.rs`（接続コード）・`candidate.rs`（候補のアドレスと経路の選び方）・`stun.rs`・`upnp.rs`・`nat.rs`（回線の型と、接続できなかったときの文）・`guard.rs`（接続してよいアドレスの範囲。既定はloopbackだけ）・`lan_addr.rs`と`lan_proto.rs`と`adapter_pick.rs`（LAN対戦）。
- **ルームサーバーのクライアント**: `room.rs`（ルームマッチ）・`room_queue.rs`（ランダムマッチ）・`watch_list.rs`（観戦の一覧）・`room_rejoin.rs`・`spectate_room.rs`。サーバーの側は`src/server`です。
- **観戦**: `spectate.rs`（観戦のパケットと、ホストと観戦者の状態機械）。
- **ゲーム内のメニュー**: `net_menu.rs`（メニューの状態機械と、画面に出す文の表）・`ui_entry.rs`・`ui_layout.rs`・`room_input.rs`・`player_name.rs`・`overlay.rs`（文字の描画の下ごしらえ）・`lang.rs`。
- **リプレイ**: `replay.rs`（平文の部分と、HiganRToolの印`HGRT`）・`replay_detail.rs`と`replay_rounds.rs`（印に足すマッチの記録）・`replay_decode.rs`（暗号化と圧縮の部分）・`replay_rewrite.rs`（チャージの設定を直したコピーを作る）・`replay_playcheck.rs`と`replay_mark_cmp.rs`と`replay_compare.rs`（照合）・`autosave.rs`と`save_skip.rs`と`vault.rs`（自動保存）・`replay_build.rs`（検査用の合成のリプレイ）。
- **花映塚の模型**: `input_model.rs`（入力の展開の規則）・`return_flow.rs`（対戦の後にメニューへ戻る道筋）・`pad_dirs.rs`（パッドの十字キー）・`render_reset.rs`・`digest.rs`。ゲームのアドレスの表はここには無く、`src/dll/src/th09_addr.rs`にあります。
- **フックの下ごしらえ**: `patch.rs`（期待バイト列の照合とdetourの組み立て）・`pe.rs`（PE32の見出しと、ゲームの同定）・`arena.rs`（rollbackのための割り当て器）。
- **フレームのペース**: `pacing.rs`・`pacing_hybrid.rs`・`pacing_vpatch.rs`（遅延を比べて測るためだけの、vpatchの待ち方を組み直したモード）。
- **設定とログ**: `ini.rs`・`ini_lenient.rs`（誤りのある行だけを捨てて読む）・`ini_edit.rs`・`ini_store.rs`・`window_size.rs`・`log_cfg.rs`・`log_days.rs`・`log_names.rs`・`log_scrub.rs`（ログに書く前に個人を特定できる文字を伏せる）・`gzlog.rs`・`exception_names.rs`。
- **アップデート**: `update.rs`（署名つきの目録の読みと、署名の確かめ。公開鍵とURLはここの1か所）・`issues.rs`（不具合の報告先）。
- **その他**: `chime.rs`と`chime_lost.rs`（知らせの音をその場で合成）・`shot.rs`（スクリーンショットのPNG）・`script.rs`と`runfile.rs`（開発用の台本と、実機の検査の定義の読み方）。

## 道具（`src/bin`）

ゲームを動かさずに、ログやリプレイを読む開発用の道具です。
配布するzipには入りません。

- `replay_playcheck`: 素の花映塚でリプレイを再生したときのログと、リプレイの印を比べて、生きた試合どおりに再生できたかを確かめます。
- `replay_mark_cmp`: 2〜3台のリプレイの印のラウンドの記録を並べて比べます。
- `replay_inputs`: リプレイからフレームごとの入力をCSVに書き出します。
- `replay_charge`: リプレイの見出しのチャージの設定を読む・書き換えたコピーを作ります。
- `pred_eval`: rollbackの予測の当たり外れを、入力の記録で数えます。
- `pacing_sum`: フレームのペースの記録を集計します。
- `return_obs`・`chime_wav`: メニューの読みの照らし合わせと、知らせの音の試し聴きです。

## ビルドと検査

```
cargo build --release -p higanr_core
cargo test -p higanr_core
```

Win32に依存しないので、Windows以外でも`cargo test`できます。
ただし、検査の一部はこの公開リポジトリに入っていない開発用のデータを読むので、ここではビルドできないものがあります（`TECHNICAL.md`の「ビルドと検査」）。
リプレイとログを読む検査は、合成のデータだけを使います。

## 使っているパッケージ

- `hmac`・`sha2`・`hkdf`・`chacha20poly1305`（RustCrypto。MIT OR Apache-2.0）: 認証タグ・鍵の導出・暗号化。
- `x25519-dalek`・`ed25519-dalek`（BSD-3-Clause）: 使い捨ての鍵の交換と、アップデートの目録の署名の確かめ。
- `flate2`（MIT OR Apache-2.0）: 大きなログのgzip。
- `png`（MIT OR Apache-2.0）: スクリーンショット。
- `encoding_rs`（(Apache-2.0 OR MIT) AND BSD-3-Clause）: ファイル名と`HiganRTool.ini`のShift_JIS。

暗号は自作していません。
