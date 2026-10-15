# src/manifest_sign

自動アップデートの目録に署名する道具です（クレート`higanr_manifest_sign`。成果物は`manifest_sign.exe`）。
配布する側だけが使い、配布するzipには入りません。

```text
manifest_sign keygen --key <秘密鍵のファイル>     鍵を作ります（既にあれば断ります）。公開鍵を出します。
manifest_sign pubkey --key <秘密鍵のファイル>     公開鍵を出します。
manifest_sign sign   --key <秘密鍵のファイル> --in <署名なしの目録> --out <署名つきの目録>
manifest_sign verify --pub <公開鍵の16進> --in <署名つきの目録>
```

- 署名はEd25519です。目録の形と確かめ方は`higanr_core::update`にあります。
- 秘密鍵のファイルは、gitで追跡しない場所に置いてください。作業ツリーの中で`.gitignore`に外されていない場所と、`dist`フォルダの下は、読むのも書くのも断ります。
- 配布するexeが信じる公開鍵は、`src/core/src/update.rs`の`UPDATE_PUBLIC_KEY_HEX`の1か所に貼ります。
- 手順: 配布物を作る手順が、zipと、更新のファイルの置き場（置き換えるファイルのコピーと、署名なしの目録`manifest.unsigned.txt`）を作ります。
  `sign`で`manifest.txt`を作り、置き場のファイルをGitHubのReleasesの最新に載せます。
  ルームサーバーの`/update/manifest`がそれを配ります（`src/server/src/update.ts`）。
- exeの名前に`update`や`setup`を使いません（Windowsがインストーラーとみなして、管理者の権限を求めることがあるためです）。

## 使っているパッケージ

- `ed25519-dalek`（BSD-3-Clause）: 署名。
- `getrandom`（MIT OR Apache-2.0）: 鍵の種の乱数。
