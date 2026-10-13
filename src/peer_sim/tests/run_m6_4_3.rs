//! 観戦者も中継越しの回（`scripts/runs/m6_4_3_*.run`）の配線の検査: 回の定義の先頭に書いた 3 つのコマンド（中継 2 本と観戦用のコードの
//! 書き換え）が、`peer_sim` の引数として読め、3 つの ini（ホスト・ゲスト・観戦者）のアドレスと向きが合っているか。
//!
//! `higanr_core::runfile::check_triple` は 3 台の ini の受信ソケット・ホストとゲストの peer までしか見ない（中継 2 本目と書き換え先は ini に無く、
//! 回の定義の先頭の文だけにある）。ここは、その先頭のコマンドをコピーして起こしたときに「ポートを取り違えて観戦者が繋がらない」「2 本の中継が
//! 同じポートを取り合う」がないことを、実機の前に安く見る。実時間も実機も使わない。期待値は回の定義の先頭の文と ini のファイルから読み、
//! 検査の中にアドレスをコピーしない（コピーすると、ini と先頭が同時にずれたときに通る）。

use std::net::SocketAddr;
use std::path::PathBuf;

use higanr_core::ini;
use peer_sim::cli::{self, Parsed, RewriteCli};
use peer_sim::shape;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    let p = root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{} が読めない: {e}", p.display()))
}

fn run_value(run: &str, key: &str) -> String {
    run.lines()
        .find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().to_string())
        })
        .unwrap_or_else(|| panic!("回の定義に {key} が無い"))
}

fn head_commands(run: &str) -> Vec<Vec<String>> {
    run.lines()
        .filter_map(|l| l.trim_start().strip_prefix('#'))
        .filter_map(|l| l.trim().strip_prefix("build\\bin\\peer_sim.exe "))
        .map(|rest| rest.split_whitespace().map(str::to_string).collect())
        .collect()
}

fn addr_of(i: &ini::Ini, key: &str, who: &str) -> SocketAddr {
    i.get("netplay", key)
        .unwrap_or_else(|| panic!("{who} の ini に {key} が無い"))
        .parse()
        .unwrap_or_else(|_| panic!("{who} の ini の {key} がアドレスとして読めない"))
}

struct Wiring {
    relay_players: shape::Args,
    relay_watch: shape::Args,
    rewrite: RewriteCli,
    host_bind: SocketAddr,
    host_peer: SocketAddr,
    guest_bind: SocketAddr,
    guest_peer: SocketAddr,
    watch_bind: SocketAddr,
    host_code_dir: String,
    watch_code_dir: String,
}

fn wiring() -> Wiring {
    let host_run = read("scripts/runs/m6_4_3_host.run");
    let guest_run = read("scripts/runs/m6_4_3_guest.run");
    let watch_run = read("scripts/runs/m6_4_3_watch.run");
    let ini_of = |run: &str| ini::parse(&read(&run_value(run, "ini"))).expect("ini が読める");
    let (h, g, w) = (ini_of(&host_run), ini_of(&guest_run), ini_of(&watch_run));

    let mut shapes = Vec::new();
    let mut rewrites = Vec::new();
    for args in head_commands(&host_run) {
        match cli::parse(&args, 1) {
            Ok(Parsed::Shape(a)) => shapes.push(*a),
            Ok(Parsed::RewriteSpectateCode(r)) => rewrites.push(*r),
            other => panic!("頭のコマンド {args:?} が読めない・想定外の形: {other:?}"),
        }
    }
    assert_eq!(
        (shapes.len(), rewrites.len()),
        (2, 1),
        "回の定義の頭のコマンドは、中継 2 本と書き換え 1 つ"
    );
    let (mut it, rewrite) = (shapes.into_iter(), rewrites.remove(0));
    Wiring {
        relay_players: it.next().unwrap(),
        relay_watch: it.next().unwrap(),
        rewrite,
        host_bind: addr_of(&h, "bind", "ホスト"),
        host_peer: addr_of(&h, "peer", "ホスト"),
        guest_bind: addr_of(&g, "bind", "ゲスト"),
        guest_peer: addr_of(&g, "peer", "ゲスト"),
        watch_bind: addr_of(&w, "bind", "観戦者"),
        host_code_dir: h
            .get("spectate", "code_dir")
            .expect("ホストの code_dir")
            .to_string(),
        watch_code_dir: w
            .get("netplay", "code_dir")
            .expect("観戦者の code_dir")
            .to_string(),
    }
}

#[test]
fn the_head_commands_pair_with_the_three_inis() {
    let w = wiring();
    let (p, s) = (&w.relay_players, &w.relay_watch);
    assert_eq!((p.a, p.b), (w.host_bind, w.guest_bind));
    assert_eq!((p.via_a, p.via_b), (w.host_peer, w.guest_peer));
    assert_eq!((s.a, s.b), (w.host_bind, w.watch_bind));
    assert_eq!(w.rewrite.candidate, s.via_b, "書き換え先は --via-b");
    assert_ne!(w.rewrite.candidate, s.via_a);
    assert!(!w.rewrite.overwrite, "前の回のコードを黙って潰さない");
    let all = [
        w.host_bind,
        w.guest_bind,
        w.watch_bind,
        p.via_a,
        p.via_b,
        s.via_a,
        s.via_b,
    ];
    for (i, x) in all.iter().enumerate() {
        assert!(!all[..i].contains(x), "アドレス {x} が重なる: {all:?}");
    }
    for (dir, strong, base) in [
        ("A→B（ホスト → 観戦者）", s.a2b, p.a2b),
        ("B→A（観戦者 → ホスト）", s.b2a, p.b2a),
    ] {
        assert!(
            strong.drop_pct > base.drop_pct && strong.jitter_ms > base.jitter_ms,
            "{dir}: 観戦者の側の乱れが 1 本目より強くない: {strong:?} / {base:?}"
        );
        assert_ne!(strong.seed, base.seed, "{dir}: seed が同じ");
    }
    assert!(s.a2b.drop_pct > 0 && s.b2a.drop_pct > 0);
    // 書き換えの入出力は、ホストと観戦者の ini の置き場（ローダーは DLL のフォルダ = build\bin からの相対）。
    let dir_of = |f: &std::path::Path| {
        f.parent()
            .and_then(|d| d.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    };
    assert_eq!(dir_of(&w.rewrite.input), Some(w.host_code_dir.clone()));
    assert_eq!(dir_of(&w.rewrite.output), Some(w.watch_code_dir.clone()));
    assert_eq!(
        w.rewrite.input.file_name().and_then(|n| n.to_str()),
        Some("spectate_code.txt")
    );
    assert_eq!(
        w.rewrite.output.file_name().and_then(|n| n.to_str()),
        Some("peer_code.txt")
    );
    assert_eq!(p.check(), Ok(()));
    assert_eq!(s.check(), Ok(()));
}
