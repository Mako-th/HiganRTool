# src/peer_sim

偽の対戦相手です（クレート`peer_sim`。成果物は`peer_sim.exe`）。
花映塚を使わずに、HiganRToolのプロトコルだけを話します。
検査のための道具で、配布するzipには入りません。

- `peer_sim`どうしを同じPCのloopbackで接続し、`src/core`の状態機械（接続 → 選択 → 試合と同期の確かめ → 切断）が本物のUDPの上で通しで動くかを見ます。
- 本物の花映塚（DLL）を相手にするときは`--follow`を付けます。試合の長さを自分で決めず、DLLが進めたフレームまでだけを使い、DLLが再戦や選択へ移ったら付いていきます。
- プロトコルの決まりはここには無く、すべて`src/core`にあります（受けたパケットの振り分けと呼ぶ順は`src/core/src/link.rs`）。ここにあるのは、道具の決め事（時計・送る入力・わざと壊す・記録・いつ終えるか）だけです。
- 接続してよいアドレスは`--scope loopback | lan | internet`で決めます（既定はloopbackだけ）。範囲の外の宛先と受信ソケットは、起動のときに断ります。名前（`localhost`など）も受けません（DNSを引くためです）。
- `unsafe`はありません。外のパッケージも使っていません（標準ライブラリと`higanr_core`だけです）。

引数の一覧は`peer_sim --help`で出ます。
細かい決まりは、各ファイルの先頭のコメント（`//!`）に書いています。

## 中身

- `src/sim.rs`: 本体です。1msごとに受信を取り出し、1フレームの枠でフレームを進め、`higanr_core::link`（DLLと同じもの）を呼びます。
- `src/cli.rs`: 起動の引数。
- `src/fault.rs`: わざと壊す（落とす・遅らせる・重ねる・揺らす・別のバージョンを名乗る）。
- `src/record.rs`: 記録（フレームごとの列と要約）。回ごとに別の名前で書き、上書きしません。花映塚のフォルダとその下には書きません。
- `src/menu_plan.rs`: 選択の台本（下の「選択の台本」）。
- `src/shape.rs`: 本物の花映塚2台の間に挟む、遅延とロスを掛ける中継（`--shape`）。
- `src/spectate_host.rs`・`src/watch.rs`・`src/spectate_code.rs`: 観戦の配る側・観戦者・観戦用のコードの書き換え。
- `src/compare.rs`・`src/compare2.rs`・`src/compare_spectate.rs`・`src/csv_input.rs`: 記録の突き合わせ（`--compare`・`--compare2`・`--compare-spectate`）。
- `src/stun_fake.rs`・`src/stun_probe.rs`: 検査用の偽のSTUNと、ルーターの見え方を測る道具。
- `src/keys.rs`: 使い捨ての鍵の種の用意。
- `tests/`: 組み上がりの検査です。実時間の待ちを使うものは`#[ignore]`にしてあり、`cargo test -p peer_sim --test loopback -- --ignored`で走らせます。

## 2つで接続してみる

同じPCでウィンドウを2つ開き、それぞれで走らせます（ポートは空いているものを使ってください）。

```
peer_sim --bind 127.0.0.1:47001 --peer 127.0.0.1:47002 --name A --seed 5 --frames 300 --latency 2 --drop 10 --delay 5 --delay-max 30 --dup 3 --fault-seed 7
peer_sim --bind 127.0.0.1:47002 --peer 127.0.0.1:47001 --name B --seed 6 --frames 300 --latency 1 --load-ms 200
```

- どちらも「全部のsegを終えた」で終わり（終了コード0）、exeと同じフォルダの`logs`に記録が1つずつできます。
- 2つの記録の`#`で始まらない行（seg・フレーム・1P・2P・ディレイ）が一致していれば、両者が同じ入力の列を使ったことになります。
- 要約に、相手を待ったフレームの数・「到着 → 適用」の時間の分布・捨てたパケットの数が出ます。
- `--nonce`を省くと、どちらがホストになるかは毎回変わります（大きい方がホストです）。
- 全部のパケットに認証タグが付きます。`--secret`を片方だけ変えると、相手のパケットはすべて捨てられ、接続の手順の時間切れで終わります。

## 回線を模す

- `--one-way-ms N`: 自分の送りを全部N ms遅らせます。送り直しも同じだけ遅れるので、相手から見た片道がN ms長くなります。
- `--jitter-ms N`: 送るコピー1通ごとに、0〜N msの乱数の遅れを足します。届く順が入れ替わります（`--jitter-shape uniform|spiky`）。
- `--drop`・`--delay`・`--delay-max`・`--dup`: パケットごとに、落とす・遅らせる・重ねる割合（%）。4msごとの送り直しの一番早いコピーで打ち消されるので、遅れを模すなら上の2つを使います。
- `--fps N`: 1フレームの枠をN fpsにします。

既定から変えた回は、要約に「検査用」の行が出ます。

## 遅延とロスを揺らす中継（`--shape`）

本物の花映塚2台（DLLどうし）の間に挟み、向きごとに遅れとロスを掛けます。
中身は読まずに運びます（認証タグで守られているので、書き換えません）。
2台の`HiganRTool.ini`の`peer`を、それぞれ中継のポートに向けます。

```
peer_sim --shape --a 127.0.0.1:47001 --b 127.0.0.1:47002 --via-a 127.0.0.1:47011 --via-b 127.0.0.1:47012 --one-way-ms 30 --drop 2
```

- `--a`・`--b`は2台の受信ソケット、`--via-a`・`--via-b`は中継のポートです（Aの`peer`に`--via-a`、Bの`peer`に`--via-b`を書きます）。4つともloopbackのアドレスです。
- 回線を模す引数は、そのまま書くと両方向に、先頭に`--a2b-`・`--b2a-`を付けるとその向きだけに掛かります（例: `--a2b-one-way-ms 60 --b2a-drop 5`）。
- 10秒ごとと終わりに、向きごとの数（受けた・落とした・遅らせた・重ねた・出した・送り元違いで捨てた）を1行出します。

## rollback方式（`--mode rollback`）

両者が`--mode rollback`を望んだときだけ、rollback方式で試合をします（片方でもdelayならdelay方式です）。
決まりは`higanr_core`の`handshake.rs`・`link.rs`・`rollback.rs`にあり、ここは呼ぶだけです。

- `--rb-delay N`（0〜9）: 入力の遅れD。ホストの値を使います。`peer_sim`は試合の途中でDを変えません。途中でDを変える経路は、DLLどうしの対戦か、`src/core/tests/rollback_sim.rs`の模型で確かめます。
- `--rb-ahead N`（1〜16）: 予測の上限。`--rb-sync on|off`: 先に進んでいる側が待って揃える時刻合わせ。
- ゲームが無いので、`peer_sim`は状態を持たず、記録・観戦・同期の確かめには確定した行だけを使います。

```
peer_sim --shape --a 127.0.0.1:47101 --b 127.0.0.1:47102 --via-a 127.0.0.1:47111 --via-b 127.0.0.1:47112 --one-way-ms 20 --jitter-ms 30 --drop 2
peer_sim --bind 127.0.0.1:47101 --peer 127.0.0.1:47111 --nonce 9000 --name A --seed 5 --mode rollback
peer_sim --bind 127.0.0.1:47102 --peer 127.0.0.1:47112 --nonce 1 --name B --seed 6 --mode rollback
```

## 観戦（`--spectate-host`・`--watch`・`--compare-spectate`）

花映塚なしで、ホスト・ゲスト・観戦者を同じPCのloopbackで回せます。
決まりは`src/core/src/spectate.rs`にあります。

```
peer_sim --bind 127.0.0.1:47301 --peer 127.0.0.1:47302 --nonce 900 --spectate-host 127.0.0.1:47310 --frames 200
peer_sim --watch --bind 127.0.0.1:47311 --code <ホストが出した観戦用のコード> --log-dir logs
peer_sim --bind 127.0.0.1:47302 --peer 127.0.0.1:47301 --nonce 100 --frames 200
peer_sim --compare-spectate logs/<観戦者の…_watch_frames.csv> logs/<ホストの記録> logs/<観戦者の…_watch_events.csv>
```

- ホストは起動のときに観戦用の接続コードを1行出します（ホストになったときだけ流します）。
- 観戦用のコードと対戦用のコードは取り違えられません（逆に渡すと断ります）。
- `--rewrite-spectate-code IN OUT --candidate ADDR`は、観戦用のコードの候補のアドレスを書き換えます。本物の観戦者（DLL）を`--shape`の中継越しに接続するときに使います。

## その他

- `--wait-sec N`: 接続の手順が時間切れになっても、起動からN秒までは作り直して待ちます（相手の花映塚の起動が遅いとき）。
- `--max-sec N`: 打ち切ります。接続していれば、相手へ終了の合図を1通出してから終えます。
- `--proto-ver N`: 別のバージョンのHiganRToolを名乗ります。バージョンの違う相手を接続の前に断る経路を、loopbackで確かめられます。
- `--stun-probe A B`: 1つの受信ソケットから2か所のSTUNサーバーへ問い合わせ、ルーターが外側のポートを宛先ごとに変えるかを出して終わります。外のサーバーへ問い合わせるので、`--scope internet`と一緒に書きます。
- `--stun-fake ADDR`: 検査用の偽のSTUNを立てて待ちます。外のSTUNサーバーへは何も送りません。

## 選択の台本（`--menu <ファイル>`）

難易度・キャラ・マップの選択を、台本どおりに進めます。
画面の移り方は`higanr_core::menu_model`（DLLと同じもの）で追います。
`--menu`が無ければ、ホストは相手を見たら「決定」を1回出します。

1行に1項目で、`#`から行末は注です。
上から順に1つずつ行います。
形は`screen=S [wait=N] 動作`です。

- **`screen=S`**（必須）: 画面がこれになってから行います。7が難易度、8がキャラ、16がマップ、6が選択の外です。
- **`wait=N`**: 画面が`S`になってからNフレーム待ちます（既定0）。
- **`word=キー[+キー…] [hold=N]`**: 自分の入力をNフレーム（既定1）これにします。キーは`Z` `X` `SHIFT` `ESC` `UP` `DOWN` `LEFT` `RIGHT`か、`0x`の16進です。画面を移るキー（Z・X・Esc）は落とします。
- **`press=decide|cancel`**: 画面を移る押下です。難易度とマップは`value=V`、キャラの決定は`cursor=C`（0〜15）と`shift=0|1`で値を決めます。
- **`enter`**: 選択の外から、自分で次の選択に入ります。

知らない名前・範囲の外の値・動作が0個か2個の行は、行の番号を添えて断り、何も送らずに終わります（終了コード2）。
細かい決まりは`src/menu_plan.rs`の先頭のコメントにあります。

見本（`tests/loopback.rs`の`MENU_HOST`・`MENU_GUEST`と同じです）:

```
# ホスト 1P
screen=7 press=cancel                    # 難易度の画面の取消で選択の外へ
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
peer_sim --bind 127.0.0.1:47101 --peer 127.0.0.1:47102 --name A --nonce 100 --seed 5 --segs 1 --frames 200 --menu host.txt
peer_sim --bind 127.0.0.1:47102 --peer 127.0.0.1:47101 --name B --nonce 1 --seed 6 --segs 1 --frames 200 --life 3 --menu guest.txt
```

両方が終了コード0で終わり、2つの記録の`# 押下`の行と、`#`で始まらない行がそれぞれ一致すれば合格です。
