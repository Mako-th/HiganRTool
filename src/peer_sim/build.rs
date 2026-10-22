//! 開発側の資料を読む検査のための cfg `dev_assets` を出す（資料の無い公開側では立たない）。

fn main() {
    higanr_build_res::emit_dev_assets_cfg();
}
