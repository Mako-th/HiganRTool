# src/manifest_sign

`manifest_sign.exe`（クレート `higanr_manifest_sign`）。配る側の道具で、**配る zip には入らない**。更新の目録（`higanr_core::update`）の
署名の鍵を作り、目録に署名し、確かめる。

```text
manifest_sign keygen --key <秘密の鍵のファイル>     鍵を作る（既にあれば断る）。公開鍵を出す
manifest_sign pubkey --key <秘密の鍵のファイル>
manifest_sign sign   --key <秘密の鍵のファイル> --in <署名なしの目録> --out <署名つきの目録>
manifest_sign verify --pub <公開鍵の16進> --in <署名つきの目録>
```

- 秘密の鍵のファイルは **git に追跡されない場所**に置く（このリポジトリでは `.gitignore` で外した `keys\`）。作業ツリーの中で `.gitignore` に外されていない場所と、`dist` フォルダの下は、読むのも書くのも断る。
- 配る手順: ① 配布物の作成の道具が zip と更新の置き場 `dist/<名前>-release/`（置き換える 5 ファイルのコピーと署名なしの目録
  `manifest.unsigned.txt`）を作る ② `manifest_sign sign --key <鍵> --in dist/<名前>-release/manifest.unsigned.txt --out dist/<名前>-release/manifest.txt`
  ③ 置き場の 6 ファイル（`manifest.txt` を含む）を配り場所（GitHub の Releases の最新）に載せる。
- exe の名前に `update`・`setup` などを含めない（Windows が「インストーラー」とみなして管理者の権限を求める。os error 740）。
- 公開鍵は `src/core/src/update.rs` の `UPDATE_PUBLIC_KEY_HEX` に貼る（配る exe が信じる鍵。1 か所）。
- 署名の部品は `ed25519-dalek`（BSD-3-Clause）、種の乱数は `getrandom`（MIT OR Apache-2.0）。
