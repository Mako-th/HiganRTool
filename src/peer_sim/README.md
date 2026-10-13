# src/peer_sim

偽の対戦相手（クレート `peer_sim`。`build.bat` が `build/bin/peer_sim.exe` へコピーする）。花映塚を使わずプロトコルだけ話す exe。
peer_sim 同士を同じ PC の loopback で繋ぎ、`src/core/` の状態機械（接続 → 選択 → lockstep ＋ CHECK、繋がった後の生存）が本物の UDP の上で通しで動くかを見る。
本物の花映塚（dll）を相手にするときは `--follow`（付いていく形）: マッチの長さを自分で決めず、dll が入力ワードを取ったフレームまでだけを使い、
dll が次の seg（再戦）か次の選択（メニューへ戻った）に移ったら seg を進める。細部は `src/sim.rs` のモジュール doc。
ネットの選択では `--menu <台本>`（下の「選択の台本」）: 選択の 3 画面を core の模型（`higanr_core::menu_model`。dll と共用）で追い、決めた MENU の入力ワードと画面を移る押下を台本どおりに出す。
`--menu` が無ければ、ホストが相手を見たら画面を持たない「決定」を 1 回出す選択になる。

- プロトコルの決まりはここに書かない（`src/core/` にだけある。受けたものの振り分けと呼ぶ順も `src/core/src/link.rs`）。ここにあるのは道具の決め事（時計・台本の入力ワード・わざと壊す・記録・いつ終えるか）。
- ネットワークの範囲（`--scope loopback | lan | internet`。既定 loopback = `127.0.0.0/8` と `[::1]`）の外の宛先と受信ソケットは起動時に断る（範囲の中身は `src/core/src/guard.rs`）。
  未指定アドレス `0.0.0.0` / `[::]`・マルチキャスト・ブロードキャスト・ポート 0 の宛先はどの範囲でも断る。名前（`localhost` など）も受けない（DNS を引くため）。
- unsafe は無い。外部のクレートも無い（std と `higanr_core` だけ）。

## 中身

| ファイル | 何を |
|---|---|
| `src/lib.rs` | 本体の入口（`tests/` から直に呼べるよう lib にした） |
| `src/menu_plan.rs` | 選択の台本の読み込み（`parse` / `load`）と、台本を 1 フレームずつ進める `Runner` |
| `src/sim.rs` | `run` / `run_with_menu`（台本つき）/ `run_opts`（台本とペース `fps`）: 1 ms ごとに受信を取り出し、1 フレームの枠（既定 60 fps。`--fps`）でフレームを進め、core の繋がりのセッション（`higanr_core::link`。dll と同じもの）を呼ぶ。付いていく形では、相手が取ったフレームまでを使えるだけ使う |
| `src/fault.rs` | わざと壊す（固定 seed で落とす・遅らせる・重ねる、決め打ちの落とし方、全部の送りに足す一定の遅れ `--one-way-ms`、コピーごとの揺らぎ `--jitter-ms`、別のバージョンを名乗る `--proto-ver`） |
| `src/record.rs` | 記録（フレームごとの列と要約）。回ごとに別名で、上書きしない。`th09.exe` のあるフォルダとその下には書かない |
| `src/cli.rs` | 起動の引数 |
| `src/stun_fake.rs` | 検査の偽の STUN の応答役（既定は loopback。`spawn_in` と `--stun-fake` で答える相手の範囲を lan にできる。送り元のアドレスを XOR-MAPPED-ADDRESS で返す。`spawn_shifted(n)` は答えのポートを n ずらして返し、宛先ごとに外側のポートを変えるルーターを模す） |
| `src/shape.rs` | `--shape`（開発用。本物の花映塚 2 台の間に挟む、遅延とロスを向きごとに掛ける UDP の中継。送り元で向きを決める `route`・向きごとの乱れ `Lane` は純粋に切り出してある） |
| `src/spectate_host.rs` | `--spectate-host`（対戦のホストの役が、core の `SpectateHost` で観戦者へ記録を流す役。別の受信ソケットを持つ） |
| `src/watch.rs` | `--watch`（観戦者の役。core の `Watcher` で JOIN・組み立て・ACK。マッチの入力ワードの CSV と記録の CSV を書く） |
| `src/compare_spectate.rs` | `--compare-spectate`（観戦者の CSV と対戦者の `_net_frames.csv` / peer_sim の記録の突き合わせ） |
| `src/stun_probe.rs` | `--stun-probe A B`（開発用。1 つの受信ソケットから 2 か所の STUN へ聞いて、見えたアドレスのポートが同じかを出して終わる） |
| `tests/loopback.rs` | 組み上がりの検査（2 つを 2 本のスレッドで loopback に立てる）。実時間の待ちを使うので `#[ignore]` で毎回の回から外してあり、全件の回（`cmd /c build.bat`）が `cargo test -p peer_sim --test loopback -- --ignored` で走らせる |

## 開発者向け: exe の 2 プロセスで確かめる（手で 1 回）

同じ PC でウィンドウを 2 つ開き、`build/bin/` で（ポートは空いているものを使う）:

```
peer_sim.exe --bind 127.0.0.1:47001 --peer 127.0.0.1:47002 --name A --seed 5 --frames 300 --latency 2 --lat 0:100:4 --drop 10 --delay 5 --delay-max 30 --dup 3 --fault-seed 7
peer_sim.exe --bind 127.0.0.1:47002 --peer 127.0.0.1:47001 --name B --seed 6 --frames 300 --latency 1 --load-ms 200
```

- 両方が「全部の seg を終えた」で終わり（終了コード 0）、`build/bin/logs/peer_sim_<UTC の日時>_<名前>_<pid>.log` が 1 つずつできる。
- 2 つの記録の `#` で始まらない行（seg frame 1P 2P delay）が一致すること: `diff <(grep -v "^#" A の記録) <(grep -v "^#" B の記録)`
- 要約（画面と記録の末尾）に、stall の総数と最大連続、「到着 → 適用」の p50 / p95 / p99 / max、捨てたものの数が出る。
- どちらがホストになるかは `--nonce` を省くと毎回変わる（大きい方がホスト。`--lat` はホストのときだけ効く）。
- 全部のパケットに認証タグが付く。`--secret` を両方で省けば既定の値で揃う。片方だけ変えると、相手のパケットは全部「タグ違い」（要約の `bad_tag`）で捨てられ、接続の手順の時間切れで終わる。
- 引数の一覧は `peer_sim.exe --help`。

## 遅れを模す・ペースを変える（検査用）

- `--one-way-ms N`（0〜1000）: 自分の送りを全部 N ms 遅らせる。送り直しのコピーも同じだけ遅れるので、相手から見た片道が N ms 長くなる（RTT を模す）。
  `--delay P --delay-max MS` はパケットごとに違う遅れを引くので、4 ms ごとの送り直しの一番早いコピーで遅れが打ち消される（実機で `--delay 100 --delay-max 60` でも dll の stall は 0 だった）。
  loopback の測り（delay 2、付いていく形の相手）では、片道 60 ms まで先に立つ側の stall は 0、65 ms から毎周期出た。
- `--jitter-ms N`（0〜1000。既定 0）: 送るコピー 1 通ごと（送り直しのコピーを含む）に、`--one-way-ms` に加えて 0〜N ms の乱数（`--fault-seed`。運命の乱数とは別の列）の遅れを足す。
  コピーごとに別々に引くので、一番早いコピーでも打ち消されず、送った順と届く順が入れ替わる（実回線のジッタを模す。`--drop` `--delay` `--dup` とは独立）。
  `--jitter-shape uniform|spiky`（既定 uniform）: uniform は 0〜N の一様、spiky は 9 割が 0〜N/10・1 割が N/2〜N。
- `--fps N`（1〜1000。既定 60）: マッチと選択の 1 フレームの枠を N fps にする（台本の `wait` もフレームで数える）。dll の `[pacing] mode = b` / `fps = N`（`scripts/m7_pacing_b_240.ini`）で速く回す回に揃える。`--follow` のマッチは dll に付いていく。
- どちらかを既定から変えた回は、要約に「★検査用: 刻み … fps / 送りに足した一定の遅れ … ms」の行が出る。

## 別のバージョンを名乗る（検査用）

`--proto-ver N`（0〜255。今のバージョン `higanr_core::wire::PROTO_VER` と違う数）: ネットワークの上でバージョン N の HiganRTool を名乗る。送るものと受けるものの両方で、ヘッダのバージョン（と HELLO の本文のバージョン）を今のバージョン ⇔ N で入れ替えてタグを付け直す（`src/fault.rs` の `Relabel`）。
セッションは今のバージョンのまま動くので、古いバージョンの物を建てずに「バージョンの違う相手とは繋がる前に断る」経路を loopback で確かめられる。相手が今のバージョンなら、両者とも 1 通目の HELLO で手順をやめ（10 秒の時間切れを待たない）、
要約に「相手の HiganRTool のバージョンが違います（自分 N・相手 今の版）。」が出る。

## 相手の起動を待つ・打ち切る

- `--wait-sec N`（1〜3600）: 接続の手順が時間切れ（10 秒）になっても、起動から N 秒までは作り直して待ち続ける（`Link::set_retry_window`。dll が 30 分待つのと同じ仕組み）。
  本物の花映塚の起動が遅い回に、peer_sim を起こす時刻を手で合わせずに済む。書かなければ 10 秒でやめる。`--max-sec` より長い値は断る（待つ間に打ち切りで切れ、どちらが効いたか記録から分からなくなるため）。
  `--max-sec` を省けば、その既定に N 秒を足す。要約に「接続の手順: … 作り直した k 回」の行が出る。
- `--max-sec` で打ち切るとき、繋がっていれば相手へ BYE(終了) を 1 通出してから終える。dll は「相手が終えた」として閉じる（黙って終えると、dll は 10 秒の沈黙の後に「切断」と見る）。
  BYE は 1 通だけで送り直さない（落ちれば dll は 10 秒で「切断」と見る）。要約の終わり方に、BYE を出したかが出る。

```
peer_sim.exe --bind 127.0.0.1:47002 --peer 127.0.0.1:47001 --follow --mask 0xC4 --wait-sec 600 --max-sec 1800
```

## ルーターの見え方を測る（`--stun-probe`。開発用）

家庭のルーターが、外へ出る通信の外側のポートを宛先ごとに変えるか（変えるなら、STUN で見えたアドレスは相手に使えない）を、花映塚を起こさずに見る。
1 つの受信ソケットから 2 つの STUN サーバーへ順に Binding を問い合わせ、見えたアドレスを 2 行と、ポートの比べを 1 行出して終わる（終了コード 0 = 2 か所とも答えた、2 = 答えが無い・引数の誤り）。
外の STUN サーバーへ問い合わせる道具なので、`--scope internet` と `--bind` だけと一緒に書く（ほかの引数は受けない）。本物の STUN サーバーへは、外へ出る通信を許可してから使う。

```
peer_sim.exe --scope internet --bind <自分の LAN のアドレス>:0 --stun-probe stun.cloudflare.com:3478 stun.l.google.com:19302
```

- 出力は `宛先1 …から見えたアドレス …` `宛先2 …` と、`ポートが同じ（宛先に依らない割り当て）` か `ポートが違う（宛先ごとに変わる割り当て）` の 1 行。IP だけ違う（宛先で出口が変わる回線）ときはその旨の 1 行。
- 名前は 2 秒で打ち切って引く。2 つの宛先が同じアドレスなら比べない（必ず「同じ」になるため）。片方でも答えが無ければ、「同じ」で終えずに誤りで終わる（250 ms ごとに 8 回、約 2 秒）。
- この結果は「ルーターが宛先に依るか」だけを言う。同じ家の中の 2 台が外側のアドレスで互いに届くか（ヘアピン）は別のことで、2 台の花映塚で見る（`scripts/hairpin_offer.ini` / `hairpin_answer.ini`）。
- 検査は loopback の偽の STUN 2 つ（`stun_fake`。片方は答えのポートをずらす）だけへ聞く。外へは出ない。

## 偽の STUN を起こす（`--stun-fake`。LAN の回。開発用）

`stun_fake` を単体で起こして待つ（終了コード 0 = `--max-sec`（既定 1800 秒、1〜7200）で終えた、2 = 引数の誤り）。答えた相手の送り元は 1 行ずつ出る。外の STUN サーバーへは何も送らない。

```
peer_sim.exe --scope lan --stun-fake 127.0.0.1:47900
```

- 答える相手の範囲は `--scope`（既定 loopback）。書かなければ LAN のアドレスの送り元には答えない。待ち受けアドレスも同じ範囲のガード（`guard::check_bind`）を通す。
- `bind = auto` の dll は LAN のアドレスから問い合わせるので、受信ソケットは `127.0.0.1` のままで `--scope lan` だけを書けば足りる。LAN のアドレスに受信ソケットを開くと Windows のファイアウォールの確かめが画面に出うる（peer_sim.exe に受信の規則は無い）ので、LAN のアドレスの受信ソケットは使わない。
- 使い方は `scripts/lan_offer.ini` / `lan_answer.ini` と `scripts/runs/lan_1_a.run` / `lan_1_b.run`（同じ PC で花映塚 2 台）。

## 遅延とロスを揺らす中継（`--shape`。開発用）

本物の花映塚 2 台（dll 同士）の間に挟み、向きごとに遅れとロスを掛ける（ロールバックと観戦をローカルで確かめる土台。peer_sim 自身の乱れは自分が送る入力ワードにしか掛からず、ゲームの状態も持たないので、巻き戻しの正しさは本物どうしの間で見る）。
中継はポートを 2 つ開く。A 向けのポート（`--via-a`）で A から受けたものを B 向けのポート（`--via-b`）から B へ、B 向けのポートで B から受けたものを A 向けのポートから A へ、中身を読まずに運ぶ（認証タグで守られているので書き換えない）。
dll の ini の `peer` を中継のポートに向けると、dll には相手が中継に見える。

```
peer_sim.exe --shape --a 127.0.0.1:47001 --b 127.0.0.1:47002 --via-a 127.0.0.1:47011 --via-b 127.0.0.1:47012 --one-way-ms 30 --drop 2
```

- `--a` / `--b`: 本物の A・B の送り元（それぞれの ini の `bind`）。`--via-a` / `--via-b`: 中継のポート（A の ini の `peer` に `--via-a`、B の ini の `peer` に `--via-b` を書く）。
  4 つとも別の loopback のアドレス（127.0.0.0/8 か `[::1]`。`--scope` は受けない）。送り元が「そのポートの相手」と違うもの（ポートの取り違え・知らない相手）は運ばずに捨て、数える。
- 乱れ: 「わざと壊す」の `--drop` `--delay` `--dup` `--delay-max` `--one-way-ms` `--fault-seed` `--drop-every` `--drop-run` `--drop-after` をそのまま書くと両方向に、先頭に `--a2b-` / `--b2a-` を付けるとその向きだけに掛かる
  （例 `--a2b-one-way-ms 60 --b2a-drop 5`）。同じ項目は向き指定が勝つ。乱数は向きごとに別の列（B→A の seed は A→B の seed に固定の値を混ぜたもの。`--b2a-fault-seed` で別に決められる）。遅らせたコピーは、遅れの来た順に出す（後のコピーに追い越される）。
- `--max-sec N`（1〜7200。既定 1800）で自分で終わる。10 秒ごとと終わりに 1 行（標準出力）: 向きごとの「受けた・落とした・遅らせた・重ねた・出した・捨てた（送り元違い）・送信失敗」。
  「遅らせた」は運命が遅らせるだった通で、一定の遅れ `--one-way-ms` だけの通は数えない。回の記録は出力をコピーして残す（ファイルには書かない）。
- 実機の回: `scripts/shape_a.ini` / `shape_b.ini`（2 台の ini。A のホスト 1P・B のゲスト 2P）と `scripts/runs/shape_1_a.run` / `shape_1_b.run`。中継 → A → B の順に起こす（中継の引数の例は回の定義の先頭）。
  検査 `net_config::tests::the_shape_inis_pair_up` が ini のアドレスと例が食い違わないかを見る。

## rollback のモード（`--mode rollback`）

決まりは `higanr_core` の `handshake.rs`（取り決め）・`link.rs`（セッション）・`rollback.rs`（状態機械）にあり、ここは呼ぶだけ（`src/sim.rs` の `play_rollback`）。

- `--mode delay|rollback`（既定 delay）。両者が rollback を望んだときだけ rollback でマッチを行う。片方でも delay なら delay 方式（要約に「モード」の行は出ない）。
- `--rb-delay N`（0〜9。既定 1）: rollback に決まったときの D（最初の seg の D）。ホストの値が使われる。seg ごとの D は、ホストの peer_sim は D を変えず最初の seg の D のまま全部の INPUT に lat として載せ、
  ゲストの peer_sim は seg の始まりでホストの INPUT を受けるまで取らない（`core::link` のセッションが `CaptureError::NoDelay` を返し、次の 1 ms にもう一度試す）。
  D をマッチとマッチの間に変える実行は、dll 側のホストの操作（Ctrl＋数字）か `src/core/tests/rollback_sim.rs` の模型で行う（peer_sim に seg ごとの D の台本は無い）。
  `--rb-ahead N`（1〜16。既定 8）: 予測の上限 R（ネットワークに載せない、各自の値）。`--rb-sync on|off`（既定 on）: 時刻合わせ（先にいる側が描画だけのフレームで揃える）。off は比べる実行のため。どれも `--mode rollback` と一緒にだけ書ける。
- rollback を望む実行は `--lat` と一緒に使わない（latency の切替は delay 方式だけ）。`--follow` とは一緒に使える。
- ゲームが無いので、ウィンドウは状態を持たない `NoGame`（コピーも書き戻しも何もしない）。seg の最後のフレームの更新が場面の要求を立てる（`--follow` では立てない。seg の終わりは相手の合図で決める）。
  記録・観戦・CHECK は確定した行だけ。要約に「モード: rollback（D …。予測・進め直し・待ちの数 …）」の 1 行が出る。
- 例（中継越し。3 つを別々に、中継 → A → B の順に起こす）:

```
peer_sim.exe --shape --a 127.0.0.1:47101 --b 127.0.0.1:47102 --via-a 127.0.0.1:47111 --via-b 127.0.0.1:47112 --one-way-ms 20 --jitter-ms 30 --drop 2
peer_sim.exe --bind 127.0.0.1:47101 --peer 127.0.0.1:47111 --nonce 9000 --name A --seed 5 --mode rollback
peer_sim.exe --bind 127.0.0.1:47102 --peer 127.0.0.1:47112 --nonce 1 --name B --seed 6 --mode rollback
```

- 付いていく形（`--follow --mode rollback`。dll の rollback の実行で、マッチの終わりが決まらない相手に付いていくため）: 付いていく側は seg の長さを決めず、相手が取ったフレーム（相手の INPUT の入力ワードの数が N + D + 1 に届いた N）だけを更新し、
  seg は相手の次の seg の INPUT・次の選択・BYE で進める（delay 方式の付いていく形と同じ規則。予測も進め直しもしない）。seg の最後のフレーム E を入力ワードの数から出して先へ走る形にしなかった理由は `src/sim.rs` のモジュール doc
  （確定した更新は取り消せず、E は次の seg の INPUT が来るまで分からない）。
  決める側が peer_sim のときは、seg の最後の更新が場面の要求を立てて待つ間と確定して出る直前に、次のフレームを取らない（取ると E + 1 の入力ワードが出て付いていく側が 1 フレーム余分に更新する）。
  本物の dll は待つ間も取るので、dll が予測で最後の更新をした回は付いていく側の記録に E + 1 の行が余分に載りうる（推測。未測。seg を進める合図は次の seg の INPUT なので止まりはしない）。
  壊すのは付いていく側の送りだけ（決める側 → 付いていく側の最後の INPUT が落ちると最後のフレームを更新できない。delay 方式と同じ）。

## 観戦（`--spectate-host` / `--watch` / `--compare-spectate`）

決まりは全部 `src/core/src/spectate.rs` にあり、ここは呼ぶだけ。花映塚（dll）なしに、peer_sim のホスト ＋ ゲスト ＋ 観戦者を同じ PC の loopback で回せる。

```
peer_sim.exe --bind 127.0.0.1:47301 --peer 127.0.0.1:47302 --nonce 900 --spectate-host 127.0.0.1:47310 --frames 200
    ⇒ 「観戦用の接続コード: …」を 1 行出す（ホストの役のときだけ流す。--nonce が大きい方がホスト）
peer_sim.exe --watch --bind 127.0.0.1:47311 --code <上のコード> --log-dir logs     （観戦者。--peer 127.0.0.1:47310 は、ホストを --spectate-fixed-key で起こしたときだけ —— 観戦者が式でホストの公開鍵を作る道。既定のホストは走るたびの種なので、コードを渡す）
peer_sim.exe --bind 127.0.0.1:47302 --peer 127.0.0.1:47301 --nonce 100 --frames 200     （ゲスト）
peer_sim.exe --compare-spectate logs/<観戦者の…_watch_frames.csv> logs/<ホストの記録.log> logs/<…_watch_events.csv>
```

- 観戦用の session・秘密は対戦のものと別（`--spectate-session` / `--spectate-secret`。既定はそれぞれ別の値で、対戦と同じ値は断る）。観戦用のコード（種別 3）は対戦用のコードと取り違えられない
  （`--watch --code` に対戦用のコードを渡すと断り、対戦の `--code` に観戦用のコードを渡しても断る）。
- 観戦者は 2 本の CSV を回ごとに別名で書く: `…_watch_frames.csv`（`seg,frame,p1,p2`。dll の `_net_frames.csv` と同じ 16 進 4 桁）と `…_watch_events.csv`（`index,type,fields`。SELECT 両席・選択の押下・MENU の入力ワード・seg の始まり・終わりを記録の番号つきで）。
- `--compare-spectate` の一致の決まり（`compare_spectate.rs` の先頭）: 観戦者の最初の行が対戦者の列のその seg の最初の行にあたり、そこから全行同じ、観戦者の後に残った対戦者の行が 1 行まで。
  遅れて入った観戦者（`--start-after-ms`）は途中の seg の始まりから始まってよい。押下は、記録の CSV（3 つ目の引数）と peer_sim の記録があれば比べる。
- 観戦者への送りにわざと壊す設定を掛ける手段は CLI に無い（検査は `SpectateHostOpts::fault` で掛ける）。CLI では `--shape` の中継を挟む。
- ホストの役でない走行は何も流さない（空の STREAM の合図だけ。要約の「観戦の口」に出る）。

## 観戦用のコードの書き換え（`--rewrite-spectate-code`）

本物の花映塚の観戦者（dll、`code_side = spectate`）を `--shape` の中継越しに繋ぐ道具。ホストの dll が書く観戦用のコード（`spectate_code.txt`）の候補はホストの待ち受けアドレスそのもので、
dll に観戦用の別のアドレスを書く欄が無い（`net_config.rs` の `SpectateSettings`）ので、観戦者は中継を通せない。候補を中継のポートへ書き換えたコードを観戦者へ渡す。

```
peer_sim.exe --rewrite-spectate-code build\bin\spec_host\spectate_code.txt build\bin\spec_watch\peer_code.txt --candidate 127.0.0.1:47022
    ⇒ 「観戦用のコードの候補を書き換えた: 127.0.0.1:47001 → 127.0.0.1:47022（session …・期限 UNIX 秒 …）」を 1 行出す
```

- 読み書きは core の `code::read_spectate` / `code::write_spectate`（コードの形をコピーしない）。session・秘密・名前・期限・種別は元のまま、検算だけが付け直される（検算はコピー間違いを見つけるもので署名ではないので、書き換えたコードは観戦者から見て正しい）。秘密は出力に出さない。
- 候補は置き換える（足さない）。観戦者の dll は候補の全部へ JOIN を送る（`spectate.rs` の `Watcher::poll`）ので、元のホストの受信ソケットを残すと中継を通らない経路ができる。
- 候補にするアドレスは中継の「観戦者向けのポート」: `--shape --a <ホストの受信ソケット> --b <観戦者の受信ソケット> --via-a X --via-b Y` なら `Y`（`--via-b`）。`X` を書くと、観戦者の送り元（`--b`）が「そのポートの相手（`--a`）」でないので中継が送り元違いとして捨て、観戦者は繋がらない。
- 断るもの: 対戦用のコード（種別 1・2。観戦者へ対戦の鍵を渡す経路になる）・壊れた文字・途中で切れた文字・期限切れ（期限は延ばさない）・ガードを通らない候補（`--scope`。既定 loopback。ポート 0 も断る）・入力と出力が同じファイル・ゲームのフォルダの中への出力・
  出力先が既にある（前の回の古いコードを残さない。置き換えるなら `--overwrite`）。書くのは新しいコード 1 行（末尾は dll の `spectate_code.txt` と同じ CRLF）。
- 実機の回: `scripts/runs/m6_4_3_host.run`（先頭に、中継 2 本の引数・書き換えの手順・アドレスの向き）。

## 選択の台本（`--menu <ファイル>`）

1 行 1 項目、`#` から行末は注。上から順に 1 つずつ行う。形は `screen=S [wait=N] 動作`。細部は `src/menu_plan.rs` のモジュール doc。

| 欄 | 意味 |
|---|---|
| `screen=S`（必須） | 模型の画面がこれになってから行う。7 = 難易度・8 = キャラ・16 = マップ・6 = 選択の外（難易度の画面の取消で出た後。動作は `enter` だけ） |
| `wait=N`（既定 0） | 項目が今の項目になり、画面が `S` になってから N フレーム待つ。途中で画面が変わったら数え直す |
| `word=キー[+キー…] [hold=N]` | MENU の自分の入力ワードを N フレーム（既定 1）これにする。キー名は `Z` `X` `SHIFT` `ESC` `UP` `DOWN` `LEFT` `RIGHT`（`+` でつなぐ）か `0x` の 16 進。送るときに `--mask` を掛け、画面を移るビット（Z・X・Esc）は落とす |
| `press=decide\|cancel` | 画面を移る押下。難易度（0〜3）・マップ（一覧の値 0〜15。14 = −1、15 = −2）の画面は `value=V`（既定 0）。キャラの画面は、決定なら `cursor=C`（0〜15）と `shift=0\|1` から自分の席の値を作り、取消はカーソル 0（`cursor` / `shift` は書けない）。押下が入ったら項目を終える |
| `enter` | 選択の外から自分で次の選択に入る（peer_sim 同士で両者が待ち合わないため。相手が先に入ったら行わずに終える） |

- 知らない名前・範囲の外・同じ名前の 2 回目・動作が 0 個か 2 個の行は、行の番号を添えて断り、1 通も送らずに終える（終了コード 2）。
- 押下が入るまで: ホストは相手が来ていれば毎フレーム押し直す。ゲストは要求が捨てられたら（相手が来ていない・ホストが別の押下を先に入れた）30 フレーム後に押し直す。
- 入った押下（自分のでも相手のでも）を模型に通し、マップが決まったら模型の値（自分の席のキャラ・色、マップ、難易度）と `--life` で SELECT を置く。模型に合わない押下が入ったら（両者の画面が分かれた）BYE(取消) を出してやめる。
- `--follow` と一緒に使うと、マッチから選択へ戻ったときは模型をキャラ選択（8）から始め、台本は続きの項目から。難易度は前の選択の値を持ち越す。
- 記録には、入った押下（`# 押下 step kind value screen`）と、選択の間に相手の入力ワードが変わったところ（`# 相手の語 step word`）を `#` の行で書く（`#` でない行はマッチの列だけ）。要約に、台本を何項目終えたか・模型の終わりの値が出る。

見本（ホスト 1P とゲスト 2P。`tests/loopback.rs` の `MENU_HOST` / `MENU_GUEST` と同じ）:

```
# ホスト 1P
screen=7 press=cancel                    # 難易度の画面の取消で選択の外へ（両者とも）
screen=8 word=RIGHT+X hold=5             # X は落ち、RIGHT だけが届く
screen=8 wait=40 press=decide cursor=2
screen=8 wait=10 press=cancel            # 決めた 1P の取り消し
screen=8 press=decide cursor=7 shift=1
screen=16 press=decide value=15          # マップ −2
```

```
# ゲスト 2P
screen=6 enter                           # 自分から次の選択に入る
screen=7 press=decide value=3
screen=8 word=0x44 hold=5
screen=8 wait=180 press=decide cursor=9
```

```
peer_sim.exe --bind 127.0.0.1:47101 --peer 127.0.0.1:47102 --name A --nonce 100 --seed 5 --segs 1 --frames 200 --menu host.txt --drop 10 --delay 5 --delay-max 30 --dup 3 --fault-seed 7
peer_sim.exe --bind 127.0.0.1:47102 --peer 127.0.0.1:47101 --name B --nonce 1 --seed 6 --segs 1 --frames 200 --life 3 --menu guest.txt
```

両方が終了コード 0 で終わり、2 つの記録の `# 押下` の行と `#` でない行がそれぞれ一致すること。
