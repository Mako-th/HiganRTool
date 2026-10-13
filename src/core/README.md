# src/core

HiganRTool の中核（クレート `higanr_core`）。**Win32 にもゲームのメモリにも触らない純粋な処理だけ**を置く:
プロトコルのシリアライズと状態機械、lockstep のフレームの式と入力リング、入力語の再構成、リプレイの解析・印・名前の無害化。

- `#![forbid(unsafe_code)]`。ネットから来るバイト列はここで読む。
- だから `cargo test` でそのまま検査できる（`build.bat` も毎回走らせる）。
- いまあるもの:
  - `frame.rs`（`apply_frame = capture_frame + delay`。原本は `docs/Design_Protocol.md` §1-1）
  - `wire.rs`（プロトコル v0 の encode / decode。原本は `docs/Design_Protocol.md` §1・§2・§2-0。状態機械は下の 3 つ）
  - `lockstep.rs`（マッチの中の lockstep の状態機械: INPUT の送受信・ack・冗長送信・latency の切替・seg。原本は `docs/Design_Protocol.md` §1-1・§2・§2-1。2 台の模型の検査は `testnet.rs`）
  - `check.rs`（CHECK の突き合わせ。直近 16 回分を両側で持つ。原本は `docs/Design_Protocol.md` §2。検査用の中身「使った (1P, 2P) の列のダイジェスト」の式 `input_digest_push` / `check_from_input_digest` もここ —— peer_sim と dll が同じものを使う。2026-09-28 に peer_sim から移した）
  - `liveness.rs`（繋がった後の生存: PING を 1 秒に 1 回、同じ session_id のものが 10 秒届かなければ BYE(切断)。原本は `docs/Design_Protocol.md` §3-1）
  - `selection.rs`（マッチの前の選択の状態機械。MENU・STEP_REQ・STEP・SELECT。原本は `docs/Design_Protocol.md` §2・§3 の 6・7、dll からの呼ばれ方はモジュールの doc）
  - `link.rs`（繋がりのセッション。受けたパケットをどの状態機械へ渡すか・どの順で呼ぶか・何を送り返すかを 1 か所に置き、dll と peer_sim が使う。送るものは `Outlet` へバイト列で。原本は `docs/Design_Architecture.md` §8-5 の M2-1）
  - `update.rs`（起動前の更新: 署名つきの目録〔Ed25519。パッケージ `ed25519-dalek`〕の形の読みと署名の確かめ・バージョンの比べ〔同じ・古いは使わない〕・置き換えてよいファイル名の表・URL・`[loader] update_check`。公開鍵と URL の定数はここの 1 か所。流れは `src/loader/src/update.rs`、署名の道具は `src/manifest_sign`、目録を作るのは `tools/make_dist.py`。原本は `docs/investigation/Auto_Update_20261008_WIP.md`）
  - `guard.rs`（宛先と待ち受けのアドレスをネットワークの範囲 `scope = loopback | lan | internet`（既定 loopback = 127.0.0.0/8 と `::1`）に限るガード。dll と peer_sim が使う。範囲は 2026-09-29 に足した）
  - `input_model.rs`（feed `0x0042ACE0` の模型。prev・リピート発火・押した瞬間・離した瞬間。原本は
    `docs/Design_Architecture.md` §8-1 と `analysis/notes/th09_input_and_wait_A0.md` §1）
  - `patch.rs`（フック地点の署名照合と detour の組み立て。原本は `docs/Design_Architecture.md` §4。
    実物の地点の表はここには無い —— `src/dll/th09_addr.rs`）
  - `pe.rs`（PE32 の見出し読み取りとゲーム同定用の `Ident` の照合。花映塚の実際の値はここには無い）
