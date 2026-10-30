//! peer_sim —— 偽の相手。花映塚を使わず、HiganRToolのプロトコルだけを話す。
//! peer_sim同士を同じPCのloopbackで接続し、`higanr_core`のステートマシン（接続 → 選択 → lockstep ＋ CHECK、接続した後の生存）が本物のUDPの上で通しで動くかを見る。
//! プロトコルの決まりはここに書かない（決まりは`higanr_core`にある。受けたパケットの振り分けと呼ぶ順は`higanr_core::link`にある）。ここはツールとしての決め事（時計のペース・台本の書式・わざと壊す・記録・いつ終えるか）だけ。
//! ネットワークの範囲（`--scope`。既定loopback）の外の宛先と受信ソケットは断る（`higanr_core::guard`）。unsafeは使わない。
//! 本体は`sim::run`（台本つきは`sim::run_with_menu`）。exe（`main.rs`）は引数を読み（`cli`）、ソケットを開き、`run_with_menu`を呼び、記録を書く（`record`）。

#![forbid(unsafe_code)]

pub mod cli;
pub mod compare;
pub mod compare2;
pub mod compare_spectate;
pub mod csv_input;
pub mod fault;
pub mod keys;
pub mod menu_plan;
pub mod record;
pub mod shape;
pub mod sim;
pub mod spectate_code;
pub mod spectate_host;
pub mod stun_fake;
pub mod stun_probe;
pub mod watch;

#[cfg(test)]
mod dev_assets_guard {
    use std::path::Path;

    /// 資料の有無は判定の関数とは別に解析のメモのフォルダだけで見る（判定が常に偽でも通らないように）。
    #[test]
    fn dev_asset_checks_are_compiled_when_the_dev_files_exist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert_eq!(
            cfg!(dev_assets),
            root.join("analysis").join("notes").is_dir(),
            "cfg dev_assets と資料の有無が食い違う"
        );
    }
}
