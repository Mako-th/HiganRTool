//! peer_sim —— 偽の対戦相手。花映塚を使わず、HiganRTool のプロトコルだけを話す。
//!
//! peer_sim 同士を同じ PC の loopback で繋ぎ、`higanr_core` の状態機械（接続 → 選択 → lockstep ＋ CHECK、
//! 繋がった後の生存）が本物の UDP の上で通しで動くかを見る。
//!
//! - プロトコルの決まりはここに書かない。決まりは `higanr_core` にあり、受けたパケットの振り分けと呼ぶ順も
//!   `higanr_core::link`（dll と同じセッション）。ここは道具としての決め事（時計のペース・台本の語・わざと壊す・記録・いつ終えるか）だけ。
//! - ネットワークの範囲（`--scope`。既定 loopback = 127.0.0.0/8 と ::1）の外の宛先と受信ソケットは断る（`higanr_core::guard`）。
//! - unsafe は使わない。
//!
//! 周辺の入口: 選択の台本（`--menu`）は `menu_plan`、dll の記録との突き合わせ（`--compare` / `--compare2`）は
//! `compare` / `compare2`、ルーター外側のポートが宛先ごとに変わるかの測定（`--stun-probe`）は `stun_probe`、
//! 本物の花映塚 2 台の間に挟む遅延・ロスの中継（`--shape`。loopback だけ）は `shape`。
//! 観戦は、ホストの役（`--spectate-host`）が `spectate_host`、観戦者の役（`--watch`）が `watch`、
//! 観戦者の列と対戦者の列の突き合わせ（`--compare-spectate`）が `compare_spectate`。観戦用のコードの候補の書き換え
//! （`--rewrite-spectate-code`。観戦者の dll を中継越しに繋ぐため）は `spectate_code`。
//!
//! 本体は `sim::run`（台本つきは `sim::run_with_menu`）。exe（`main.rs`）は引数を読み（`cli`）、ソケットを開き、
//! 走らせ、記録を書く（`record`）。
//! 本体は `sim::run`（台本つきは `sim::run_with_menu`）。exe（`main.rs`）は引数を読み（`cli`）、ソケットを開き、
//! `run_with_menu` を呼び、記録を書く（`record`）。

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
