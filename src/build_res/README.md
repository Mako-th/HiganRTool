# src/build_res

クレート `higanr_build_res`。配る exe・DLL（`src/loader` の HiganRTool.exe、`src/dll` の HiganRTool.dll、`src/replay_fix` の
HiganRTool_replay_fix.exe）に付けるバージョン情報（VS_VERSIONINFO）の `.res` を作る。各クレートの `build.rs` だけが
build-dependency として使い、成果物には入らない。

- MSVC でリンクするときだけ、`OUT_DIR/versioninfo.res` を書いて `cargo:rustc-link-arg-bins`（DLL は `-cdylib`）で link.exe に渡す。
  link.exe は `.res` をそのまま受け付けるので、rc.exe も Windows SDK の場所探しも要らない。mingw（Linux の検査用）では何もしない。
- バージョンは呼び出し側の `CARGO_PKG_VERSION`（ルートの `Cargo.toml` の `[workspace.package] version`）から作る。FileVersion は `1.0.0.0`、ProductVersion は `1.0.0`。
- 製品名・会社名・著作権表示は `src/lib.rs` の定数 1 か所。成果物ごとに違う欄（FileDescription / InternalName / OriginalFilename / ファイルの種類）は各 `build.rs` が渡す。
- 検査: `cargo test -p higanr_build_res`。`tests/against_llvm_rc.rs` が llvm-rc の出力とバイト単位で比べる（llvm-rc が無い環境では飛ばす。`REQUIRE_LLVM_RC=1` なら無いこと自体を失敗にする）。
