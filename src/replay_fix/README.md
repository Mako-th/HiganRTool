# src/replay_fix

リプレイの修正の道具です（クレート`higanrtool_replay_fix`。成果物は`replay_fix.exe`で、配布するときは`HiganRTool_replay_fix.exe`に名前を変えます）。
v1.0.0で保存したリプレイのうち、2人のチャージの設定（slow / charge）が違う対戦のものは、見出しにその人のPCの設定が入っているため、再生するとずれます。
この道具は、見出しのチャージの設定を直したコピーを作ります。
元のファイルは書き換えません。

## 使い方

- ウィンドウで、直すリプレイかフォルダを選び（exeへのドラッグ＆ドロップもできます）、1P・2Pのチャージの設定を選んで作ります。
- コピーは、元のフォルダの1つ上の`fix_replay\<元のフォルダ名>\<1Pの設定>-<2Pの設定>`（例: `Replay_HiganRTool\fix_replay\Alice vs Bob\charge-slow`）に、同じ名前で作ります。
  元のフォルダに1つ上が無いとき（ドライブの直下など）は、元のフォルダの中の`fix_replay\<1Pの設定>-<2Pの設定>`に作ります。
  元のリプレイが`fix_replay`の下にあるときは、`fix_replay`を重ねず、元のフォルダの中に`<1Pの設定>-<2Pの設定>`を作ります。
  フォルダをたどるときは、`fix_replay`と`<1Pの設定>-<2Pの設定>`の名前のフォルダの中は読みません。
  同じ名前のファイルが既にあれば上書きしません。
- 作った後に、「何件中何件を処理したか」「保存先のフォルダ」「修正が不要でスキップしたリプレイ」をウィンドウに出します（`--batch`では`--log`のファイルに同じ文を書きます）。
- HiganRToolの印（`HGRT`）が無いリプレイと、印に直し済みの記録（`charge`）があるリプレイは作りません。
- v1.0.0より新しいバージョンで保存したリプレイ（印のビルド番号が411272より大きいもの）も作りません。このずれはv1.0.0までのリプレイだけで起きるためです。
- ウィンドウを出さずに作るときは`--batch`を使います。引数の一覧は`replay_fix --help`で出ます。

```
replay_fix [--lang ja|en] [リプレイかフォルダ]...
replay_fix --batch <1Pの設定>-<2Pの設定> [--log ファイル] [--lang ja|en] <リプレイかフォルダ>...
```

終了コードは、0が完了、1が作れなかったものがある、2が使い方の誤り、3がリプレイが見つからない、です。

## 中身

- `src/model.rs`: 画面の状態と、出力先の名前の決まり（Win32にもファイルにも触りません）。
- `src/scan.rs`: 選ばれたファイルとフォルダから、直すリプレイの一覧を作ります。
- `src/work.rs`: リプレイを読み、直したコピーを書きます。見出しの書き換えは`higanr_core::replay_rewrite`です。
- `src/text.rs`: 画面の文（日本語・英語）。
- `src/cli.rs`: 引数と`--batch`。
- `src/win.rs`と`src/win/`: Windowsだけの部分です。ウィンドウはWindows標準の部品（BUTTON・COMBOBOX・STATIC）で作っています。
- `build.rs`: MSVCでリンクするときだけ、標準の部品を今の見た目で出すマニフェスト（コモンコントロールv6）をexeに埋めます。

## ビルドと検査

```
cargo build --release -p higanrtool_replay_fix
cargo test -p higanrtool_replay_fix
```

ウィンドウのほかの部分はWin32に依存しないので、Windows以外でも`cargo test`できます。

## 使っているパッケージ

- `windows-sys`（MIT OR Apache-2.0）: Win32の宣言だけです（Windows向けのビルドだけ）。
