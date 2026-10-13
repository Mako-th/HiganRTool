//! peer_sim.exe —— 偽の対戦相手。引数を読み（`cli`）、ネットワークの範囲（`--scope`。既定 loopback）の中の受信ソケットにソケットを開き、`sim::run` を回し、
//! 記録を書く（`record`）。使い方は `src/peer_sim/README.md`。
//!
//! パニックしない作り: 失敗は全部 Result で受け、文と終了コードで返す。終了コード: 0 = 全部の seg を終えた（`--follow` では相手の BYE(終了) を
//! 受けて終えた）/ 1 = 走ったが終えられなかった / 2 = 引数・ソケット・記録の失敗。`--compare` と `--compare2` と `--compare-spectate` は
//! 0 = 一致 / 1 = 食い違い（どちらかが 0 行で比べられていないときも）/ 2 = 読めない。`--watch`（観戦者の役）は 0 = ホストが BYE(終了) の
//! 記録を流すところまで受けた / 1 = それ以外の終わり（ホストが黙った・断られた・打ち切り・別の理由の BYE）/ 2 = 引数・ソケット・記録の失敗。
//! `--stun-probe` は走らせずに 2 か所の STUN へ聞いて終わる: 0 = 2 か所とも答えた / 2 = 答えが無い・引数の誤り。

use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};

use peer_sim::cli::{self, Parsed};
use peer_sim::compare;
use peer_sim::compare_spectate;
use peer_sim::compare2;
use peer_sim::csv_input;
use peer_sim::menu_plan;
use peer_sim::record;
use peer_sim::shape;
use peer_sim::sim::{self, End};
use peer_sim::spectate_code;
use peer_sim::spectate_host::SpectateHostOpts;
use peer_sim::stun_fake::{self, StunFake};
use peer_sim::stun_probe;
use peer_sim::watch::{self, WatchEnd};

/// 引数で渡さなかったときの role_nonce（接続ごとに 1 回引く乱数の代わり。暗号には使わない）。同じ PC で 2 つを同時に起こしても重ならないよう、
/// 時刻（ns）とプロセス番号を混ぜる。
fn default_nonce() -> u64 {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut z = t ^ (u64::from(std::process::id()) << 32);
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z ^ (z >> 31)
}

fn default_log_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("exe のパスが取れない: {e}"))?;
    let dir = exe.parent().ok_or("exe のフォルダが分からない")?;
    Ok(dir.join("logs"))
}

/// `--stun-fake`: 偽の STUN を立て、`max_sec` 秒か落とされるまで待つ。答えが増えたときに相手の送り元を 1 行出す
/// （LAN の回で「LAN のアドレスから来た」ことの証拠）。何も記録のファイルには書かない。
fn serve_stun_fake(a: &stun_fake::Args) -> Result<(), String> {
    let f = StunFake::spawn_in(a.bind, a.scope, 0)?;
    println!(
        "peer_sim: 偽の STUN を {} に立てた（答える相手の範囲 {}。{} 秒か落とされるまで）",
        f.addr(),
        a.scope.name(),
        a.max_sec
    );
    let t0 = std::time::Instant::now();
    let mut seen = 0;
    while t0.elapsed().as_secs() < u64::from(a.max_sec) {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let n = f.answered();
        if n != seen {
            seen = n;
            if let Some(from) = f.last_from() {
                println!("peer_sim: 答えた {n} 件（最後の相手 {from}）");
            }
        }
    }
    println!("peer_sim: 偽の STUN を閉じる（答えた {seen} 件）");
    Ok(())
}

/// `--shape`: 遅延とロスを揺らす中継を `max_sec` 秒（か落とされるまで）走らせ、10 秒ごとと終わりに向きごとの数を 1 行出す。
/// 何も記録のファイルには書かない（数は標準出力の行だけ。回の記録に残したいときは呼び出し側が出力をコピーする）。
fn serve_shape(a: &shape::Args) -> Result<(), String> {
    println!(
        "peer_sim: 中継 A {} ⇄ [{} 中継 {}] ⇄ B {}（{} 秒か落とされるまで）",
        a.a, a.via_a, a.via_b, a.b, a.max_sec
    );
    println!(
        "peer_sim: 乱れ A→B: {} / B→A: {}",
        shape::describe(&a.a2b),
        shape::describe(&a.b2a)
    );
    println!(
        "peer_sim: A の ini の peer = {} / B の ini の peer = {}",
        a.via_a, a.via_b
    );
    let stop = AtomicBool::new(false);
    shape::run(a, &stop, &mut |l| println!("{l}"))?;
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `--watch`: 観戦者を走らせ、要約を出し、マッチの入力ワードの CSV と記録の CSV を回ごとに別名で書く。
/// 戻り値は「ホストが BYE(終了) を流すところまで受けたか」。
fn watch_main(w: &cli::WatchCli) -> Result<bool, String> {
    let log_dir = match &w.log_dir {
        Some(d) => d.clone(),
        None => default_log_dir()?,
    };
    record::refuse_game_folder(&log_dir)?;
    let sock = UdpSocket::bind(w.bind).map_err(|e| format!("{} で待てない: {e}", w.bind))?;
    println!(
        "peer_sim: 観戦者 {} → {:?}（名前 {:?}、最初の JOIN は {} ms 後）",
        w.bind, w.cfg.host, w.cfg.name, w.cfg.start_after_ms
    );
    let rep = watch::run(&w.cfg, sock, w.scope).map_err(|e| e.to_string())?;
    print!("{}", watch::summary(&rep));
    let frames = record::write_named(
        &log_dir,
        &w.cfg.name,
        "_watch_frames.csv",
        &watch::frames_csv(&rep),
    )?;
    let events = record::write_named(
        &log_dir,
        &w.cfg.name,
        "_watch_events.csv",
        &watch::events_csv(&rep),
    )?;
    println!("記録: {}\n記録: {}", frames.display(), events.display());
    Ok(matches!(
        rep.end,
        WatchEnd::HostEnded(higanr_core::wire::ByeReason::Finished)
    ))
}

fn compare_spectate_main(
    watch: &PathBuf,
    player: &PathBuf,
    events: Option<&PathBuf>,
) -> Result<bool, String> {
    let read = |p: &PathBuf| csv_input::read_csv_text(p);
    let w = compare_spectate::parse_watch_frames(&read(watch)?)
        .map_err(|e| format!("{}: {e}", watch.display()))?;
    let ptext = read(player)?;
    let p =
        compare_spectate::parse_player(&ptext).map_err(|e| format!("{}: {e}", player.display()))?;
    // 押下は、観戦者の記録の CSV があり、対戦者が peer_sim の記録（`# 押下` の行を持つ）のときだけ比べる。
    let steps = match events {
        Some(e) => {
            let ws =
                watch::parse_events_csv(&read(e)?).map_err(|m| format!("{}: {m}", e.display()))?;
            let ps = compare_spectate::presses_from_peer_log(&ptext);
            if ptext.contains("# 押下") {
                Some((ws.steps, ps, ws.first_index == Some(0)))
            } else {
                println!("押下: 対戦者の記録に押下の行が無い（dll の CSV など）ので比べない");
                None
            }
        }
        None => None,
    };
    let (text, ok) = compare_spectate::report(
        &w,
        &p,
        steps
            .as_ref()
            .map(|(a, b, from_start)| (a.as_slice(), b.as_slice(), *from_start)),
    );
    print!("{text}");
    Ok(ok)
}

fn real_main() -> Result<bool, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let c = match cli::parse(&args, default_nonce()) {
        Ok(Parsed::Run(c)) => *c,
        Ok(Parsed::Watch(w)) => return watch_main(&w),
        Ok(Parsed::RewriteSpectateCode(r)) => {
            let done = spectate_code::rewrite_files(
                &r.input,
                &r.output,
                r.candidate,
                r.scope,
                r.overwrite,
                now_unix(),
            )?;
            println!("{}", spectate_code::summary(&done));
            println!("書いた: {}", r.output.display());
            return Ok(true);
        }
        Ok(Parsed::CompareSpectate(w, p, e)) => {
            return compare_spectate_main(&w, &p, e.as_ref());
        }
        Ok(Parsed::Help) => {
            print!("{}", cli::USAGE);
            return Ok(true);
        }
        Ok(Parsed::Compare(dll, peer)) => {
            let read = |p: &PathBuf| csv_input::read_csv_text(p);
            let d = compare::parse_dll_csv(&read(&dll)?)?;
            let p = compare::parse_peer_record(&read(&peer)?)?;
            let (text, ok) = compare::report(&d, &p);
            print!("{text}");
            return Ok(ok);
        }
        Ok(Parsed::Compare2(a, b)) => {
            let read = |p: &PathBuf| csv_input::read_csv_text(p);
            let ra =
                compare::parse_dll_csv(&read(&a)?).map_err(|e| format!("{}: {e}", a.display()))?;
            let rb =
                compare::parse_dll_csv(&read(&b)?).map_err(|e| format!("{}: {e}", b.display()))?;
            let (text, ok) =
                compare2::report2(&ra, &rb, &a.display().to_string(), &b.display().to_string());
            print!("{text}");
            return Ok(ok);
        }
        Ok(Parsed::StunProbe(a)) => {
            // 走らせずに 2 か所の STUN へ聞いて終わる（`stun_probe`）。答えが無ければ Err（終了コード 2）。
            let sock =
                UdpSocket::bind(a.bind).map_err(|e| format!("{} で待てない: {e}", a.bind))?;
            let r = stun_probe::run(&a, &sock, |h, p| {
                stun_probe::lookup_with_timeout(h, p, stun_probe::DNS_TIMEOUT)
            })?;
            print!("{}", stun_probe::report_text(&a, &r));
            return Ok(true);
        }
        Ok(Parsed::StunFake(a)) => {
            serve_stun_fake(&a)?;
            return Ok(true);
        }
        Ok(Parsed::Shape(a)) => {
            serve_shape(&a)?;
            return Ok(true);
        }
        Err(e) => return Err(format!("{e}\n\n{}", cli::USAGE)),
    };
    let log_dir = match &c.log_dir {
        Some(d) => d.clone(),
        None => default_log_dir()?,
    };
    // 走らせる前に置き場を確かめる（実行し終えてから書けないと分かるのを避ける）。
    record::refuse_game_folder(&log_dir)?;
    // 選択の台本は繋ぐ前に読む（誤りがあれば 1 通も送らずに断る）。
    let menu = match &c.menu {
        Some(p) => Some(menu_plan::load(p)?),
        None => None,
    };
    let sock = UdpSocket::bind(c.bind).map_err(|e| format!("{} で待てない: {e}", c.bind))?;
    if c.sim.follow {
        println!(
            "peer_sim: {} → {}（名前 {:?}、相手の試合の長さに付いていく）",
            c.bind, c.sim.peer, c.sim.name
        );
    } else {
        println!(
            "peer_sim: {} → {}（名前 {:?}、seg {} × {} フレーム）",
            c.bind, c.sim.peer, c.sim.name, c.sim.segs, c.sim.frames_per_seg
        );
    }
    if let Some(m) = &menu {
        println!("peer_sim: 選択の台本 {} 項目", m.items.len());
    }
    // rollback を望む実行だけ 1 行出す（既定の delay 方式の実行の出力は変えない）。
    if c.mode.mode == higanr_core::wire::Mode::Rollback {
        println!(
            "peer_sim: rollback を望む（D {}・R {}・時刻合わせ {}。相手も望めば rollback、でなければ delay 方式）",
            c.mode.rollback_delay,
            c.ahead,
            if c.time_sync { "あり" } else { "なし" }
        );
    }
    if c.fps != higanr_core::pacing::DEFAULT_FPS || c.sim.fault.one_way_ms != 0 {
        println!(
            "peer_sim: ★検査用: ペース {} fps / 送りに足す一定の遅れ {} ms",
            c.fps, c.sim.fault.one_way_ms
        );
    }
    if let Some(w) = c.wait_ms {
        println!(
            "peer_sim: 接続の手順は時間切れの後も {} 秒まで作り直して待つ（打ち切りは {} 秒）",
            w / 1_000,
            c.sim.max_ms / 1_000
        );
    }
    // 範囲を広げた実行だけ 1 行出す（既定の loopback の実行の出力は変えない）。
    if c.scope != higanr_core::guard::Scope::Loopback {
        println!("peer_sim: ネットワークの範囲 {}", c.scope.name());
    }
    // 観戦のポート（`--spectate-host`）。観戦用の接続コードを 1 行出す（観戦者の `--watch --code` へ渡す）。
    let spectate = match &c.spectate {
        Some(s) => {
            let sp_sock = UdpSocket::bind(s.bind)
                .map_err(|e| format!("観戦の口 {} で待てない: {e}", s.bind))?;
            println!("peer_sim: 観戦の口 {}（ホストの役のときだけ流す）", s.bind);
            println!(
                "peer_sim: 観戦用の接続コード: {}",
                cli::spectate_code(s, &c.sim.name, now_unix())?
            );
            Some(SpectateHostOpts {
                sock: sp_sock,
                session_id: s.session_id,
                secret: s.secret,
                key_seed: s.key_seed,
                fault: peer_sim::fault::FaultConfig::default(),
            })
        }
        None => None,
    };
    let rep = sim::run_opts(
        &c.sim,
        sock,
        sim::RunOpts {
            menu: menu.as_ref(),
            fps: c.fps,
            wait_ms: c.wait_ms,
            scope: c.scope,
            spectate: spectate.as_ref(),
            mode: c.mode,
            ahead: c.ahead,
            time_sync: c.time_sync,
        },
    )
    .map_err(|e| e.to_string())?;
    let summary = record::summary(&rep);
    print!("{summary}");
    let text = record::log_text(&rep, &args.join(" "));
    let path = record::write_log(&log_dir, &c.sim.name, &text)?;
    println!("記録: {}", path.display());
    Ok(matches!(rep.end, End::Finished { .. }))
}

fn main() -> ExitCode {
    match real_main() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("peer_sim: {e}");
            ExitCode::from(2)
        }
    }
}
