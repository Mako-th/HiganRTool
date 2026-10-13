//! 実機の回の定義（`scripts/runs/*.run`）の組の検査（`higanr_core::runfile`）。
//!
//! 1 本目はリポジトリの定義を全部読み、組の誤りが 0 であることを見る（実機の回の前に、ここで落とす）。残りは合成の定義
//! （中身はメモリの上）で、決まりを 1 つずつ破って、その決まりで落ちることを見る。古い台本と `--menu` の無い引数の組は、
//! リポジトリの本物の台本と ini を指す合成の定義で落ちることを見る。

use higanr_core::mode_pick::Shape;
use higanr_core::net_menu::{Cmd, Conn, Menu, Note, RoomItem, TextIn, TickIn};
use higanr_core::room_input::RoomBuf;
use higanr_core::runfile::{Game, Problem, Rule, RunDef, check, check_at, parse};
use higanr_core::wire::{Mode, SeatWish};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_repo(root: &Path) -> impl Fn(&str) -> Option<String> + '_ {
    move |p: &str| std::fs::read_to_string(root.join(p)).ok()
}

#[test]
fn every_run_definition_in_the_repository_is_consistent() {
    let root = repo_root();
    let dir = root.join("scripts/runs");
    let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{} を開けない: {e}", dir.display()))
        .map(|e| e.expect("scripts/runs/ の項目").path())
        .filter(|p| p.extension().is_some_and(|x| x == "run"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "{} に定義が 1 本も無い", dir.display());
    let read = read_repo(&root);
    let mut errors = Vec::new();
    let mut net_runs = 0;
    for p in &names {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(p)
            .unwrap_or_else(|e| panic!("{} を UTF-8 で読めない: {e}", p.display()));
        match parse(&text) {
            Err(e) => errors.push(format!("{name}: {e}")),
            Ok(def) => {
                if def.peer_sim.is_some() {
                    net_runs += 1;
                }
                // 自分のパスを渡す（second / watcher / watching の先が自分を指し返しているかを見るため）。
                for pr in check_at(Some(&format!("scripts/runs/{name}")), &def, &read) {
                    errors.push(format!("{name}: {pr}"));
                }
            }
        }
    }
    assert!(errors.is_empty(), "回の定義の誤り:\n{}", errors.join("\n"));
    for must in [
        "m7_game.run",
        "m7_b.run",
        "m5b_03_long.run",
        "m6_3_watch.run",
        "m6_4_1_host.run",
        "m6_4_1_guest.run",
        "m6_4_1_watch.run",
        "m6_4_2_host.run",
        "m6_4_2_guest.run",
        "m6_4_2_watch.run",
        "m6_4_3_host.run",
        "m6_4_3_guest.run",
        "m6_4_3_watch.run",
        "m8_4_d0_a.run",
        "m8_4_d0_b.run",
        "m8_4_delay_a.run",
        "m8_4_delay_b.run",
        "m1c3_0_entry.run",
        "m1c3_1_host.run",
        "m1c3_1_guest.run",
        "m1c3_2_human_a.run",
        "m1c3_2_human_b.run",
    ] {
        assert!(
            names
                .iter()
                .any(|p| p.file_name().is_some_and(|n| n == must)),
            "scripts/runs/{must} が無い"
        );
    }
    assert!(net_runs > 0, "ネットワークの回の定義が 1 本も無い");
}

#[test]
fn the_failed_m5b_run3_combination_is_rejected() {
    let root = repo_root();
    let text = "\
script = scripts/m2_01_net_match_and_rematch.txt
ini = scripts/m5b_netplay.ini
peer_sim = --bind 127.0.0.1:47002 --peer 127.0.0.1:47001 --name sim --nonce 1 --latency 2 --host-seat 2 --seed 0x51 --follow --mask 0xC4 --life 1
role = host
game = a
limit_min = 16
purpose = 2026-09-28 16:19 の誤った組
";
    let def = parse(text).expect("parse");
    let probs = check(&def, &read_repo(&root));
    let rules: Vec<Rule> = probs.iter().map(|p| p.rule).collect();
    assert!(rules.contains(&Rule::Menu), "{probs:?}");
    assert!(rules.contains(&Rule::OldSelectScript), "{probs:?}");
    assert!(
        probs
            .iter()
            .all(|p| matches!(p.rule, Rule::Menu | Rule::OldSelectScript)),
        "ほかの決まりでも落ちている（合成の組が今日の失敗の形からずれている）: {probs:?}"
    );
}

const SCRIPT: &str = "\
scene=1 screen=8 sub=1 wait=30 p1=Z note=char_1p
scene=2 loading=0 wait=180 limit=3600 note=match_start
wait=600 p1=Z note=tap
";

const NET_INI: &str = "\
[netplay]
enable = 1
bind = 127.0.0.1:47001
peer = 127.0.0.1:47002
session = 0x48475254
secret = 000102030405060708090a0b0c0d0e0f
nonce = 0xF000
host_seat = 1
[replay]
enable = 1
";

const OFF_INI: &str = "[replay]\nenable = 1\n";

const PLAN: &str = "# 注\nscreen=8 wait=200 press=decide cursor=1\n";

const PEER: &str = "--bind 127.0.0.1:47002 --peer 127.0.0.1:47001 --nonce 1 --host-seat 2 --follow --mask 0xC4 --life 1 --menu scripts/t.plan";

fn run_text(peer: Option<&str>, extra: &str) -> String {
    let mut t = String::from(
        "script = scripts/t.txt\nini = scripts/t.ini\ngame = a\nlimit_min = 5\npurpose = 合成\n",
    );
    if let Some(p) = peer {
        t.push_str(&format!("peer_sim = {p}\n"));
    }
    t.push_str(extra);
    t
}

fn files(script: &str, ini: &str, plan: &str) -> HashMap<String, String> {
    HashMap::from([
        ("scripts/t.txt".to_string(), script.to_string()),
        ("scripts/t.ini".to_string(), ini.to_string()),
        ("scripts/t.plan".to_string(), plan.to_string()),
    ])
}

fn check_with(run: &str, fs: &HashMap<String, String>) -> Vec<Problem> {
    let def = parse(run).unwrap_or_else(|e| panic!("{run}\n{e}"));
    check(&def, &|p: &str| fs.get(p).cloned())
}

/// 誤りが `rule` の 1 種類だけであること（ほかの決まりに巻き込まれて落ちたのではないこと）を見る。
fn only(probs: &[Problem], rule: Rule) {
    assert!(!probs.is_empty(), "{rule:?} で落ちるはずが通った");
    assert!(
        probs.iter().all(|p| p.rule == rule),
        "{rule:?} だけのはずが: {probs:?}"
    );
}

#[test]
fn good_synthetic_runs_pass() {
    let fs = files(SCRIPT, NET_INI, PLAN);
    let p = check_with(&run_text(Some(PEER), "role = host\n"), &fs);
    assert!(p.is_empty(), "{p:?}");
    let fs = files(SCRIPT, OFF_INI, PLAN);
    let p = check_with(&run_text(None, ""), &fs);
    assert!(p.is_empty(), "{p:?}");
}

#[test]
fn two_game_runs_point_at_each_other() {
    let a = run_text(None, "role = host\nsecond = scripts/runs/b.run\n");
    let b = run_text(None, "role = guest\nsecond = scripts/runs/a.run\n")
        .replace("game = a", "game = b");
    let mut fs = files(SCRIPT, NET_INI, PLAN);
    fs.insert("scripts/runs/b.run".to_string(), b.clone());
    let p = check_with(&a, &fs);
    assert!(p.is_empty(), "{p:?}");
    let mut gone = fs.clone();
    gone.remove("scripts/runs/b.run");
    only(&check_with(&a, &gone), Rule::Missing);
    let mut same = fs.clone();
    same.insert(
        "scripts/runs/b.run".to_string(),
        b.replace("game = b", "game = a"),
    );
    only(&check_with(&a, &same), Rule::NetPairing);
    for other in [
        b.replace("role = guest", "role = host"),
        b.replace("role = guest\n", ""),
    ] {
        let mut r = fs.clone();
        r.insert("scripts/runs/b.run".to_string(), other);
        only(&check_with(&a, &r), Rule::Role);
    }
    let mut one = fs.clone();
    one.insert(
        "scripts/runs/b.run".to_string(),
        b.replace("second = scripts/runs/a.run\n", ""),
    );
    only(&check_with(&a, &one), Rule::NetPairing);
    let with_peer = run_text(Some(PEER), "role = host\nsecond = scripts/runs/b.run\n");
    only(&check_with(&with_peer, &fs), Rule::NetPairing);
    let off = files(SCRIPT, OFF_INI, PLAN);
    let mut off = off.clone();
    off.insert("scripts/runs/b.run".to_string(), b.clone());
    only(&check_with(&a, &off), Rule::NetPairing);
    for bad in ["b", "scripts/t.txt", "scripts/runs/../x.run", "C:/x.run"] {
        let t = a.replace("second = scripts/runs/b.run", &format!("second = {bad}"));
        assert!(parse(&t).is_err(), "{bad} を通した");
    }
}

#[test]
fn choose_needs_a_window_and_a_typable_pick() {
    use higanr_core::mode_pick::Pick;
    const CODE_INI: &str = "[netplay]\nenable = 1\nbind = 127.0.0.1:47001\ncode_side = offer\nnonce = 0xF000\nhost_seat = 1\n";
    let a = |choose: &str| {
        run_text(
            None,
            &format!("role = host\nsecond = scripts/runs/b.run\n{choose}"),
        )
    };
    let b = run_text(None, "role = guest\nsecond = scripts/runs/a.run\n")
        .replace("game = a", "game = b");
    let mut fs = files(SCRIPT, CODE_INI, PLAN);
    fs.insert("scripts/runs/b.run".to_string(), b);
    for (v, want) in [
        ("0", Pick::Rollback(0)),
        ("2", Pick::Rollback(2)),
        ("delay", Pick::Delay),
        ("DELAY", Pick::Delay),
        ("\u{FF11}", Pick::Rollback(1)),
    ] {
        let run = a(&format!("choose = {v}\n"));
        assert_eq!(parse(&run).unwrap().choose, Some(want), "{v}");
        let p = check_with(&run, &fs);
        assert!(p.is_empty(), "{v}: {p:?}");
    }
    assert_eq!(parse(&a("")).unwrap().choose, None);
    let mut no_side = fs.clone();
    no_side.insert(
        "scripts/t.ini".to_string(),
        CODE_INI.replace("code_side = offer\n", ""),
    );
    only(&check_with(&a("choose = 0\n"), &no_side), Rule::Choose);
    let with_peer = run_text(Some(PEER), "role = host\nchoose = 1\n");
    only(
        &check_with(&with_peer, &files(SCRIPT, NET_INI, PLAN)),
        Rule::Choose,
    );
    let off = run_text(None, "choose = delay\n");
    only(
        &check_with(&off, &files(SCRIPT, OFF_INI, PLAN)),
        Rule::Choose,
    );
    for bad in ["9", "rollback", "10", "-1", "1 2", "d"] {
        assert!(
            parse(&a(&format!("choose = {bad}\n"))).is_err(),
            "{bad:?} を通した"
        );
    }
    assert!(parse(&a("choose = 0\nchoose = 1\n")).is_err());
    assert!(parse(&a("choose =\n")).is_err());
}

#[test]
fn a_run_without_a_script() {
    let run =
        run_text(Some(PEER), "role = host\n").replace("script = scripts/t.txt", "script = none");
    let mut fs = files(SCRIPT, NET_INI, PLAN);
    fs.remove("scripts/t.txt");
    let asked = std::cell::RefCell::new(Vec::new());
    let def = parse(&run).unwrap_or_else(|e| panic!("{run}\n{e}"));
    assert_eq!(def.script, None);
    let p = check(&def, &|q: &str| {
        asked.borrow_mut().push(q.to_string());
        fs.get(q).cloned()
    });
    assert!(p.is_empty(), "{p:?}");
    assert!(
        !asked.borrow().iter().any(|q| q == "none"),
        "none を読みに行った: {:?}",
        asked.borrow()
    );
    let mut fs2 = fs.clone();
    fs2.remove("scripts/t.plan");
    only(&check_with(&run, &fs2), Rule::Missing);
    for bad in ["None", "none.txt", "scripts/none/"] {
        let t = run.replace("script = none", &format!("script = {bad}"));
        assert!(parse(&t).is_err(), "{bad} を通した");
    }
}

#[test]
fn missing_references() {
    let run = run_text(Some(PEER), "");
    for gone in ["scripts/t.txt", "scripts/t.ini", "scripts/t.plan"] {
        let mut fs = files(SCRIPT, NET_INI, PLAN);
        fs.remove(gone);
        let p = check_with(&run, &fs);
        assert!(p.iter().any(|x| x.rule == Rule::Missing), "{gone}: {p:?}");
    }
    let fs = files("scene=1 bogus=1\n", NET_INI, PLAN);
    only(&check_with(&run, &fs), Rule::Missing);
    let p = check_with(
        &run_text(Some(&PEER.replace("scripts/t.plan", "C:/t.plan")), ""),
        &files(SCRIPT, NET_INI, PLAN),
    );
    only(&p, Rule::Missing);
}

#[test]
fn net_and_peer_sim_go_together() {
    only(
        &check_with(&run_text(None, ""), &files(SCRIPT, NET_INI, PLAN)),
        Rule::NetPairing,
    );
    only(
        &check_with(&run_text(Some(PEER), ""), &files(SCRIPT, OFF_INI, PLAN)),
        Rule::NetPairing,
    );
    let off = "[netplay]\nenable = 0\n";
    only(
        &check_with(&run_text(Some(PEER), ""), &files(SCRIPT, off, PLAN)),
        Rule::NetPairing,
    );
    only(
        &check_with(
            &run_text(None, "role = host\n"),
            &files(SCRIPT, OFF_INI, PLAN),
        ),
        Rule::NotNet,
    );
}

const GAME_INI: &str = "\
[netplay]
enable = 1
bind = 127.0.0.1:47403
code_side = game
clipboard = file
code_dir = code_game_entry
";

#[test]
fn a_game_menu_run_without_a_peer_is_allowed_only_for_the_game_side() {
    let fs = files(SCRIPT, GAME_INI, PLAN);
    let p = check_with(&run_text(None, ""), &fs);
    assert!(p.is_empty(), "{p:?}");
    only(
        &check_with(&run_text(None, "role = host\n"), &fs),
        Rule::NotNet,
    );
    only(
        &check_with(&run_text(None, "choose = 1\n"), &fs),
        Rule::Choose,
    );
    for side in ["offer", "answer"] {
        let ini = GAME_INI.replace("code_side = game", &format!("code_side = {side}"));
        let probs = check_with(&run_text(None, ""), &files(SCRIPT, &ini, PLAN));
        assert!(
            probs.iter().any(|p| p.rule == Rule::NetPairing),
            "{side}: {probs:?}"
        );
    }
    let probs = check_with(&run_text(Some(PEER), ""), &fs);
    assert!(
        !probs.is_empty(),
        "peer_sim つきの game の回が決まりを通らずに通った"
    );
}

#[test]
fn menu_is_required_for_net_runs() {
    let fs = files(SCRIPT, NET_INI, PLAN);
    only(
        &check_with(
            &run_text(Some(&PEER.replace(" --menu scripts/t.plan", "")), ""),
            &fs,
        ),
        Rule::Menu,
    );
    let fs = files(
        SCRIPT,
        NET_INI,
        "screen=16 wait=60 press=decide value=0\nscreen=8 word=RIGHT\n",
    );
    only(&check_with(&run_text(Some(PEER), ""), &fs), Rule::Menu);
    let old = SCRIPT.replace(
        "note=char_1p\n",
        "note=char_1p\nscene=1 screen=8 wait=30 p2=Z note=char_2p\n",
    );
    let fs = files(&old, NET_INI, PLAN);
    only(
        &check_with(&run_text(Some(PEER), ""), &fs),
        Rule::OldSelectScript,
    );
}

#[test]
fn follow_and_mask() {
    let fs = files(SCRIPT, NET_INI, PLAN);
    only(
        &check_with(&run_text(Some(&PEER.replace(" --follow", "")), ""), &fs),
        Rule::Follow,
    );
    for m in [
        "",
        " --mask 0xC5",
        " --mask 0xD4",
        " --mask 0xE4",
        " --mask 0xFFFF",
        " --mask q",
    ] {
        let peer = PEER.replace(" --mask 0xC4", m);
        only(&check_with(&run_text(Some(&peer), ""), &fs), Rule::Mask);
    }
}

#[test]
fn own_clock_needs_one_long_seg_instead_of_follow() {
    let fs = files(SCRIPT, NET_INI, PLAN);
    let own = PEER.replace(" --follow", " --segs 1 --frames 18000");
    let reason = "own_clock = 合成\n";
    assert_eq!(
        check_with(&run_text(Some(&own), reason), &fs),
        vec![],
        "正しい own_clock の回"
    );
    only(&check_with(&run_text(Some(&own), ""), &fs), Rule::Follow);
    for peer in [
        format!("{own} --follow"),
        own.replace("--segs 1", "--segs 2"),
        own.replace(" --segs 1", ""),
        own.replace(" --frames 18000", ""),
        own.replace("--frames 18000", "--frames 17999"),
        own.replace("--frames 18000", "--frames x"),
    ] {
        only(
            &check_with(&run_text(Some(&peer), reason), &fs),
            Rule::Follow,
        );
    }
}

#[test]
fn role_follows_the_nonces() {
    let fs = files(SCRIPT, NET_INI, PLAN);
    only(
        &check_with(&run_text(Some(PEER), "role = guest\n"), &fs),
        Rule::Role,
    );
    only(
        &check_with(
            &run_text(Some(&PEER.replace("--nonce 1", "--nonce 0xF000")), ""),
            &fs,
        ),
        Rule::Role,
    );
    only(
        &check_with(&run_text(Some(&PEER.replace("--nonce 1 ", "")), ""), &fs),
        Rule::Role,
    );
    let fs = files(SCRIPT, &NET_INI.replace("nonce = 0xF000\n", ""), PLAN);
    only(&check_with(&run_text(Some(PEER), ""), &fs), Rule::Role);
    // peer_sim の nonce が大きければ dll はゲスト。role = guest と合い、席は peer_sim の --host-seat 2 で 1P。
    let fs = files(SCRIPT, NET_INI, PLAN);
    let p = check_with(
        &run_text(
            Some(&PEER.replace("--nonce 1", "--nonce 0x10000")),
            "role = guest\n",
        ),
        &fs,
    );
    assert!(p.is_empty(), "{p:?}");
}

#[test]
fn dll_is_player_one() {
    let fs = files(
        SCRIPT,
        &NET_INI.replace("host_seat = 1", "host_seat = 2"),
        PLAN,
    );
    only(&check_with(&run_text(Some(PEER), ""), &fs), Rule::Seat);
    let fs = files(SCRIPT, &NET_INI.replace("host_seat = 1\n", ""), PLAN);
    only(&check_with(&run_text(Some(PEER), ""), &fs), Rule::Seat);
    let fs = files(SCRIPT, NET_INI, PLAN);
    let guest = PEER.replace("--nonce 1", "--nonce 0x10000");
    only(
        &check_with(
            &run_text(Some(&guest.replace("--host-seat 2", "--host-seat 1")), ""),
            &fs,
        ),
        Rule::Seat,
    );
    only(
        &check_with(
            &run_text(Some(&guest.replace(" --host-seat 2", "")), ""),
            &fs,
        ),
        Rule::Seat,
    );
}

#[test]
fn addresses_session_and_secret() {
    let fs = files(SCRIPT, NET_INI, PLAN);
    for bad in [
        PEER.replace("--bind 127.0.0.1:47002", "--bind 127.0.0.1:47003"),
        PEER.replace("--peer 127.0.0.1:47001", "--peer 127.0.0.1:47009"),
    ] {
        only(&check_with(&run_text(Some(&bad), ""), &fs), Rule::Address);
    }
    only(
        &check_with(&run_text(Some(&format!("{PEER} --session 0x1")), ""), &fs),
        Rule::Session,
    );
    let p = check_with(
        &run_text(Some(&format!("{PEER} --session 0x48475254")), ""),
        &fs,
    );
    assert!(p.is_empty(), "{p:?}");
    only(
        &check_with(
            &run_text(
                Some(&format!("{PEER} --secret 000102030405060708090a0b0c0d0eff")),
                "",
            ),
            &fs,
        ),
        Rule::Secret,
    );
    let fs = files(
        SCRIPT,
        &NET_INI.replace("secret = 000102030405060708090a0b0c0d0e0f\n", ""),
        PLAN,
    );
    only(&check_with(&run_text(Some(PEER), ""), &fs), Rule::Secret);
}

#[test]
fn lat_plan_only_on_the_host() {
    let lat_ini = NET_INI.replace("host_seat = 1\n", "host_seat = 1\nlat_plan = 1500:4\n");
    let fs = files(SCRIPT, &lat_ini, PLAN);
    assert!(check_with(&run_text(Some(PEER), ""), &fs).is_empty());
    let guest = PEER.replace("--nonce 1", "--nonce 0x10000");
    only(&check_with(&run_text(Some(&guest), ""), &fs), Rule::LatPlan);
    let fs = files(SCRIPT, NET_INI, PLAN);
    assert!(check_with(&run_text(Some(&format!("{guest} --lat 0:900:4")), ""), &fs).is_empty());
    only(
        &check_with(&run_text(Some(&format!("{PEER} --lat 0:900:4")), ""), &fs),
        Rule::LatPlan,
    );
}

#[test]
fn life_is_required_for_net_autosave() {
    let peer = PEER.replace(" --life 1", "");
    let fs = files(SCRIPT, NET_INI, PLAN);
    only(&check_with(&run_text(Some(&peer), ""), &fs), Rule::Life);
    let fs = files(SCRIPT, &NET_INI.replace("[replay]\nenable = 1\n", ""), PLAN);
    assert!(check_with(&run_text(Some(&peer), ""), &fs).is_empty());
}

#[test]
fn up_and_down_after_the_match_starts() {
    let fs_off = |s: &str| files(s, OFF_INI, PLAN);
    let run = run_text(None, "");
    for bad in [
        "wait=600 p1=DOWN note=d\n",
        "wait=600 p2=UP note=u\n",
        "scene=2 pause=0 wait=10 p1=Z+SHIFT+UP note=m\n",
        "wait=600 p1=DOWN\n",
    ] {
        let s = format!("{SCRIPT}{bad}");
        only(&check_with(&run, &fs_off(&s)), Rule::EndMenuUpDown);
    }
    for ok in [
        "scene=2 pause=1 wait=30 p1=DOWN note=pause_down\n",
        "scene=1 screen=16 sub=1 wait=30 p1=DOWN note=map_down\n",
    ] {
        let s = format!("{SCRIPT}{ok}");
        let p = check_with(&run, &fs_off(&s));
        assert!(p.is_empty(), "{ok:?}: {p:?}");
    }
    let s = format!("scene=1 screen=1 sub=1 cursor=0 wait=60 p1=DOWN note=title\n{SCRIPT}");
    assert!(check_with(&run, &fs_off(&s)).is_empty());
    let s = format!("{SCRIPT}wait=600 p1=DOWN note=d\n");
    assert!(check_with(&run_text(None, "allow_updown = d\n"), &fs_off(&s)).is_empty());
    only(
        &check_with(&run_text(None, "allow_updown = tap\n"), &fs_off(SCRIPT)),
        Rule::StaleAllowance,
    );
    only(
        &check_with(&run_text(None, "allow_updown = nothing\n"), &fs_off(SCRIPT)),
        Rule::StaleAllowance,
    );
}

#[test]
fn limit_covers_the_script() {
    // SCRIPT の下限は (30 + 1) + 180 + (600 + 1) = 812 tick = 13 秒。1 分は足りる。
    let long = format!("{SCRIPT}wait=36000 note=ten_min\n");
    let fs = files(&long, OFF_INI, PLAN);
    let run = run_text(None, "").replace("limit_min = 5", "limit_min = 10");
    only(&check_with(&run, &fs), Rule::Limit);
    let run = run_text(None, "").replace("limit_min = 5", "limit_min = 11");
    assert!(check_with(&run, &fs).is_empty());
}

const WATCH_SCRIPT: &str = "\
scene=1 screen=1 sub=1 cursor=0 wait=60 limit=3600 p1=DOWN note=title_down1
scene=1 screen=6 sub=1 cursor=0 wait=30 p1=Z note=vs_human
";

const WATCH_INI: &str = "\
[netplay]
enable = 1
bind = 127.0.0.1:47301
code_side = spectate
code_dir = spec_watch
after_script = keep
";

const WATCH_HOST: &str = "--bind 127.0.0.1:47101 --peer 127.0.0.1:47102 --nonce 0x20000 --segs 1 --frames 600 --mask 0xC4 --menu scripts/t.plan --spectate-host 127.0.0.1:47111 --wait-sec 60";

const WATCH_GUEST: &str = "--bind 127.0.0.1:47102 --peer 127.0.0.1:47101 --nonce 1 --segs 1 --frames 600 --mask 0xC4 --menu scripts/t.plan";

fn watch_run(host: &str, guest: &str, extra: &str) -> String {
    let mut t = run_text(Some(host), extra);
    t.push_str(&format!("peer_sim2 = {guest}\n"));
    t
}

/// 観戦者の回の誤りが `rule` の 1 種類だけで出ること。`ini` と `script` は差し替える（既定は正しい組）。
fn watch_only(run: &str, script: &str, ini: &str, rule: Rule) {
    only(&check_with(run, &files(script, ini, PLAN)), rule);
}

#[test]
fn a_good_watch_run_passes() {
    let fs = files(WATCH_SCRIPT, WATCH_INI, PLAN);
    let p = check_with(&watch_run(WATCH_HOST, WATCH_GUEST, ""), &fs);
    assert!(p.is_empty(), "{p:?}");
    let d = parse(&watch_run(WATCH_HOST, WATCH_GUEST, "")).unwrap();
    assert_eq!(
        d.peer_sim2.map(|a| a.len()),
        Some(WATCH_GUEST.split(' ').count())
    );
    let t = run_text(None, &format!("peer_sim2 = {WATCH_GUEST}\n"));
    assert!(parse(&t).is_err());
}

#[test]
fn a_watch_run_needs_the_ini_that_keeps_measuring() {
    let run = watch_run(WATCH_HOST, WATCH_GUEST, "");
    for ini in [
        WATCH_INI.replace("after_script = keep\n", ""),
        WATCH_INI.replace("after_script = keep", "after_script = bye"),
        WATCH_INI.replace("code_dir = spec_watch\n", ""),
        WATCH_INI.replace("enable = 1", "enable = 0"),
    ] {
        watch_only(&run, WATCH_SCRIPT, &ini, Rule::Watch);
    }
    for extra in ["role = host\n", "second = scripts/runs/b.run\n"] {
        let run = watch_run(WATCH_HOST, WATCH_GUEST, extra);
        assert!(
            check_with(&run, &files(WATCH_SCRIPT, WATCH_INI, PLAN))
                .iter()
                .any(|p| p.rule == Rule::Watch && p.key == "role"),
            "{extra:?} を通した"
        );
    }
}

#[test]
fn a_watch_script_presses_only_esc_in_the_watching_steps() {
    let run = watch_run(WATCH_HOST, WATCH_GUEST, "");
    for cond in [
        "scene=1 screen=7 sub=1",
        "scene=1 screen=8 sub=1",
        "scene=1 screen=16 sub=1",
        "scene=2 loading=0",
        "scene=2",
        "pause=1",
        "endmenu=1",
        "menuflag=1",
        "talk=0",
    ] {
        for keys in ["p1=Z", "p2=X", "p1=SHIFT+LEFT", "p1=ESC+Z", "p2=Z+ESC"] {
            let s = format!("{WATCH_SCRIPT}{cond} wait=10 {keys} note=bad\n");
            watch_only(&run, &s, WATCH_INI, Rule::Watch);
        }
    }
    let s = format!("{WATCH_SCRIPT}scene=2 wait=600 note=in_match\nwait=1 p1=Z note=after\n");
    watch_only(&run, &s, WATCH_INI, Rule::Watch);
    // 隣の肯定形: 押さない段階・Esc だけの段階は通す（p1 にも p2 にも）。後ろの条件の無い段階が Esc だけなのも通す。
    for ok in [
        "scene=1 screen=7 sub=1 wait=30 note=wait_only\n",
        "scene=2 loading=0 wait=300 note=match\n",
        "scene=2 wait=600 p1=ESC note=watch_quit\n",
        "scene=2 wait=600 p2=ESC note=watch_quit\n",
        "scene=1 screen=8 sub=1 wait=30 p1=ESC p2=ESC note=quit_in_selection\n",
        "scene=2 wait=600 note=in_match\nwait=1 p1=ESC note=quit\n",
        "endmenu=1 wait=10 note=end\n",
    ] {
        let s = format!("{WATCH_SCRIPT}{ok}");
        let p = check_with(&run, &files(&s, WATCH_INI, PLAN));
        assert!(p.is_empty(), "{ok:?}: {p:?}");
    }
    let none = run.replace("script = scripts/t.txt", "script = none");
    let p = check_with(&none, &files(WATCH_SCRIPT, WATCH_INI, PLAN));
    assert!(p.is_empty(), "{p:?}");
}

#[test]
fn a_watch_script_may_press_any_key_after_an_esc_in_the_watching_steps() {
    let run = watch_run(WATCH_HOST, WATCH_GUEST, "");
    // 観戦中の Esc の後ろ: 一時停止の中で下・Z・上・Z、条件の無い段階の Z は通す（上下は一時停止の中の段階だけ —— マッチの中の
    // 条件の無い上下は `EndMenuUpDown` が断る）。
    let ok = format!(
        "{WATCH_SCRIPT}scene=2 wait=600 note=in_match\n\
         scene=2 pause=0 wait=60 p1=ESC note=pause\n\
         scene=2 pause=1 wait=30 p1=DOWN note=down\n\
         scene=2 pause=1 wait=30 p1=Z note=to_title\n\
         scene=2 pause=1 wait=30 p1=UP note=up\n\
         wait=30 p1=Z note=yes\n"
    );
    let p = check_with(&run, &files(&ok, WATCH_INI, PLAN));
    assert!(p.is_empty(), "{p:?}");
    let bad = format!(
        "{WATCH_SCRIPT}scene=2 wait=600 p1=Z note=before\n\
         scene=2 wait=60 p1=ESC note=pause\n\
         wait=30 p1=Z note=after\n"
    );
    watch_only(&run, &bad, WATCH_INI, Rule::Watch);
    let p = check_with(&run, &files(&bad, WATCH_INI, PLAN));
    assert_eq!(p.len(), 1, "{p:?}");
}

#[test]
fn a_watch_script_does_not_press_esc_before_the_watching_steps() {
    let run = watch_run(WATCH_HOST, WATCH_GUEST, "");
    for bad in [
        "wait=10 p1=ESC note=leave\n",
        "wait=10 p2=Z+ESC note=leave\n",
        "scene=1 screen=6 sub=1 wait=10 p1=ESC note=title_esc\n",
        "scene=1 screen=1 sub=1 wait=10 p1=X+ESC note=title_esc\n",
    ] {
        let s = format!("{WATCH_SCRIPT}{bad}");
        watch_only(&run, &s, WATCH_INI, Rule::Watch);
    }
    // Esc の前が観戦中に効く段階だけなら通す（順序: 観戦中の段階の後ろは観戦中）。逆順（Esc が先）は断る。
    let after = format!("{WATCH_SCRIPT}scene=2 wait=10 note=m\nwait=10 p1=ESC note=q\n");
    assert!(check_with(&run, &files(&after, WATCH_INI, PLAN)).is_empty());
    let before = format!("{WATCH_SCRIPT}wait=10 p1=ESC note=q\nscene=2 wait=10 note=m\n");
    watch_only(&run, &before, WATCH_INI, Rule::Watch);
}

#[test]
fn the_two_peer_sims_of_a_watch_run_pair_up() {
    let bad = |host: &str, guest: &str| {
        watch_only(
            &watch_run(host, guest, ""),
            WATCH_SCRIPT,
            WATCH_INI,
            Rule::Watch,
        )
    };
    watch_only(
        &run_text(Some(WATCH_HOST), ""),
        WATCH_SCRIPT,
        WATCH_INI,
        Rule::Watch,
    );
    let h = |from: &str, to: &str| WATCH_HOST.replace(from, to);
    let g = |from: &str, to: &str| WATCH_GUEST.replace(from, to);
    bad(&h(" --spectate-host 127.0.0.1:47111", ""), WATCH_GUEST);
    bad(
        WATCH_HOST,
        &format!("{WATCH_GUEST} --spectate-host 127.0.0.1:47112"),
    );
    for segs in ["--segs 2", "--segs 0"] {
        bad(&h("--segs 1", segs), WATCH_GUEST);
        bad(WATCH_HOST, &g("--segs 1", segs));
    }
    bad(&h(" --segs 1", ""), WATCH_GUEST);
    bad(WATCH_HOST, &g(" --segs 1", ""));
    bad(&format!("{WATCH_HOST} --follow"), WATCH_GUEST);
    bad(WATCH_HOST, &format!("{WATCH_GUEST} --follow"));
    bad(&h(" --wait-sec 60", ""), WATCH_GUEST);
    bad(&h("--nonce 0x20000", "--nonce 1"), WATCH_GUEST);
    bad(
        &h("--nonce 0x20000", "--nonce 0x20"),
        &g("--nonce 1", "--nonce 0x20"),
    );
    bad(&h("--nonce 0x20000 ", ""), WATCH_GUEST);
    bad(
        WATCH_HOST,
        &g("--peer 127.0.0.1:47101", "--peer 127.0.0.1:47109"),
    );
    bad(
        &h("--peer 127.0.0.1:47102", "--peer 127.0.0.1:47109"),
        WATCH_GUEST,
    );
    bad(
        &h("--bind 127.0.0.1:47101", "--bind 127.0.0.1:47109"),
        WATCH_GUEST,
    );
    bad(&format!("{WATCH_HOST} --session 0x1"), WATCH_GUEST);
    bad(
        &format!("{WATCH_HOST} --session 0x1"),
        &format!("{WATCH_GUEST} --session 2"),
    );
    bad(
        &format!("{WATCH_HOST} --secret 000102030405060708090a0b0c0d0e0f"),
        &format!("{WATCH_GUEST} --secret 000102030405060708090a0b0c0d0e0e"),
    );
    let same = watch_run(
        &format!("{WATCH_HOST} --session 0x10 --secret 000102030405060708090a0b0c0d0eAB"),
        &format!("{WATCH_GUEST} --session 16 --secret 000102030405060708090a0b0c0d0eab"),
        "",
    );
    let p = check_with(&same, &files(WATCH_SCRIPT, WATCH_INI, PLAN));
    assert!(p.is_empty(), "{p:?}");
    bad(WATCH_HOST, &format!("{WATCH_GUEST} --code AAAA"));
}

#[test]
fn the_four_sockets_of_a_watch_run_are_all_different() {
    let run = watch_run(WATCH_HOST, WATCH_GUEST, "");
    for taken in ["127.0.0.1:47111", "127.0.0.1:47101", "127.0.0.1:47102"] {
        let ini = WATCH_INI.replace("127.0.0.1:47301", taken);
        watch_only(&run, WATCH_SCRIPT, &ini, Rule::Watch);
    }
    watch_only(
        &run,
        WATCH_SCRIPT,
        &WATCH_INI.replace("127.0.0.1:47301", "localhost:47301"),
        Rule::Watch,
    );
    let same = WATCH_HOST.replace(
        "--spectate-host 127.0.0.1:47111",
        "--spectate-host 127.0.0.1:47102",
    );
    watch_only(
        &watch_run(&same, WATCH_GUEST, ""),
        WATCH_SCRIPT,
        WATCH_INI,
        Rule::Watch,
    );
}

#[test]
fn a_watch_run_keeps_the_mask_and_the_menu_rules_for_both_peer_sims() {
    let with = |host: &str, guest: &str| {
        check_with(
            &watch_run(host, guest, ""),
            &files(WATCH_SCRIPT, WATCH_INI, PLAN),
        )
    };
    for m in ["--mask 0xFFFF", "--mask 0xC5"] {
        let hp = with(&WATCH_HOST.replace("--mask 0xC4", m), WATCH_GUEST);
        only(&hp, Rule::Mask);
        assert!(hp.iter().all(|p| p.key == "peer_sim"), "{hp:?}");
        let gp = with(WATCH_HOST, &WATCH_GUEST.replace("--mask 0xC4", m));
        only(&gp, Rule::Mask);
        assert!(gp.iter().all(|p| p.key == "peer_sim2"), "{gp:?}");
    }
    let hp = with(&WATCH_HOST.replace(" --mask 0xC4", ""), WATCH_GUEST);
    only(&hp, Rule::Mask);
    let gp = with(
        WATCH_HOST,
        &WATCH_GUEST.replace(" --menu scripts/t.plan", ""),
    );
    only(&gp, Rule::Menu);
    assert!(gp.iter().all(|p| p.key == "peer_sim2"), "{gp:?}");
    let fs = files(
        WATCH_SCRIPT,
        WATCH_INI,
        "screen=16 wait=60 press=decide value=0\n",
    );
    only(
        &check_with(&watch_run(WATCH_HOST, WATCH_GUEST, ""), &fs),
        Rule::Menu,
    );
}

#[test]
fn peer_sim2_is_only_for_watch_runs() {
    let run = run_text(
        Some(PEER),
        &format!("peer_sim2 = {WATCH_GUEST}\nrole = host\n"),
    );
    let p = check_with(&run, &files(SCRIPT, NET_INI, PLAN));
    assert!(
        p.iter()
            .any(|x| x.rule == Rule::Watch && x.key == "peer_sim2"),
        "{p:?}"
    );
}

const T_HOST_RUN: &str = "script = scripts/t.txt\nini = scripts/h.ini\ngame = a\nlimit_min = 5\npurpose = 合成\nrole = host\nsecond = scripts/runs/g.run\nwatcher = scripts/runs/w.run\n";
const T_GUEST_RUN: &str = "script = scripts/t.txt\nini = scripts/g.ini\ngame = b\nlimit_min = 5\npurpose = 合成\nrole = guest\nsecond = scripts/runs/h.run\n";
const T_WATCH_RUN: &str = "script = scripts/w.txt\nini = scripts/w.ini\ngame = c\nlimit_min = 5\npurpose = 合成\nwatching = scripts/runs/h.run\n";

const T_HOST_INI: &str = "\
[netplay]
enable = 1
bind = 127.0.0.1:47001
peer = 127.0.0.1:47002
session = 0x48475254
secret = 000102030405060708090a0b0c0d0e0f
nonce = 0xF000
host_seat = 1
[spectate]
enable = 1
code_dir = spec_host
";

const T_GUEST_INI: &str = "\
[netplay]
enable = 1
bind = 127.0.0.1:47002
peer = 127.0.0.1:47001
session = 0x48475254
secret = 000102030405060708090a0b0c0d0e0f
nonce = 0x0F00
host_seat = 1
";

fn triple() -> HashMap<String, String> {
    HashMap::from(
        [
            ("scripts/runs/h.run", T_HOST_RUN),
            ("scripts/runs/g.run", T_GUEST_RUN),
            ("scripts/runs/w.run", T_WATCH_RUN),
            ("scripts/t.txt", SCRIPT),
            ("scripts/w.txt", WATCH_SCRIPT),
            ("scripts/h.ini", T_HOST_INI),
            ("scripts/g.ini", T_GUEST_INI),
            ("scripts/w.ini", WATCH_INI),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string())),
    )
}

/// `path` のファイルの `from`（ちょうど 1 か所）を `to` に替えたコピーを返す。1 か所でなければ止める（同じ文字が別の場所にもあると、
/// 別の所を壊した変異になって、狙った決まりが効いているのか分からなくなるため）。
fn edit(fs: &HashMap<String, String>, path: &str, from: &str, to: &str) -> HashMap<String, String> {
    let text = &fs[path];
    assert_eq!(
        text.matches(from).count(),
        1,
        "{path} に {from:?} がちょうど 1 か所ではない"
    );
    let mut out = fs.clone();
    out.insert(path.to_string(), text.replace(from, to));
    out
}

/// 3 つの定義それぞれを自分のパスつきで検査した誤り。fs に無い定義は飛ばす（無いことを試す変異のため）。
fn triple_problems(fs: &HashMap<String, String>) -> Vec<(&'static str, Problem)> {
    let mut all = Vec::new();
    for (who, path) in [
        ("host", "scripts/runs/h.run"),
        ("guest", "scripts/runs/g.run"),
        ("watch", "scripts/runs/w.run"),
    ] {
        let Some(text) = fs.get(path) else { continue };
        let def = parse(text).unwrap_or_else(|e| panic!("{path}: {e}"));
        for p in check_at(Some(path), &def, &|q: &str| fs.get(q).cloned()) {
            all.push((who, p));
        }
    }
    all
}

/// `who` の定義が出した誤りの文に `sub` を含むものがあるか（同じ決まりを別の定義が代わりに見つけて、片方のゲートが黙っていても
/// 通ってしまうのを防ぐ）。
fn has(p: &[(&'static str, Problem)], who: &str, sub: &str) -> bool {
    p.iter().any(|(w, x)| *w == who && x.msg.contains(sub))
}

fn triple_only(fs: &HashMap<String, String>, rules: &[Rule]) {
    let p = triple_problems(fs);
    assert!(!p.is_empty(), "{rules:?} で落ちるはずが通った");
    for r in rules {
        assert!(p.iter().any(|(_, x)| x.rule == *r), "{r:?} が出ない: {p:?}");
    }
    for (who, x) in &p {
        assert!(rules.contains(&x.rule), "{who} に想定外の {x}: {p:?}");
    }
}

#[test]
fn a_good_triple_passes() {
    let fs = triple();
    let p = triple_problems(&fs);
    assert!(p.is_empty(), "{p:?}");
    let relay = edit(
        &fs,
        "scripts/h.ini",
        "peer = 127.0.0.1:47002",
        "peer = 127.0.0.1:47011",
    );
    let relay = edit(
        &relay,
        "scripts/g.ini",
        "peer = 127.0.0.1:47001",
        "peer = 127.0.0.1:47012",
    );
    let p = triple_problems(&relay);
    assert!(p.is_empty(), "{p:?}");
    let games: Vec<Game> = ["h", "g", "w"]
        .iter()
        .map(|n| parse(&fs[&format!("scripts/runs/{n}.run")]).unwrap().game)
        .collect();
    assert_eq!(games, [Game::A, Game::B, Game::C]);
}

#[test]
fn the_three_definitions_point_at_each_other() {
    let fs = triple();
    let mut gone = fs.clone();
    gone.remove("scripts/runs/w.run");
    triple_only(&gone, &[Rule::Missing]);
    let bad = edit(
        &fs,
        "scripts/runs/w.run",
        "watching = scripts/runs/h.run",
        "watching = scripts/runs/g.run",
    );
    triple_only(&bad, &[Rule::NetPairing]);
    let p = triple_problems(&bad);
    assert!(
        has(&p, "host", "を指していない"),
        "ホストの側が見ない: {p:?}"
    );
    assert!(
        has(&p, "watch", "watcher が無い"),
        "観戦者の側が見ない: {p:?}"
    );
    // ホストの側だけが気づく形: 観戦者の watching が、同じ形の別のホストの定義（h2）を指す。h2 は観戦者を指し返しているので
    // 観戦者の側は通り、ホスト h の側だけが「指し返されていない」と言う。
    let mut two_hosts = fs.clone();
    two_hosts.insert("scripts/runs/h2.run".into(), T_HOST_RUN.to_string());
    let bad = edit(
        &two_hosts,
        "scripts/runs/w.run",
        "watching = scripts/runs/h.run",
        "watching = scripts/runs/h2.run",
    );
    triple_only(&bad, &[Rule::NetPairing]);
    let p = triple_problems(&bad);
    assert!(has(&p, "host", "を指していない"), "{p:?}");
    assert!(!p.iter().any(|(who, _)| *who == "watch"), "{p:?}");
    // 観戦者の側だけが気づく形: ホストの watcher が、同じ形の別の観戦者の定義（w2。h を指す）を指す。w2 は h を指し返して
    // いるのでホストの側は通り、観戦者 w の側だけが「ホストの watcher が自分を指していない」と言う。
    let mut two_watchers = fs.clone();
    two_watchers.insert("scripts/runs/w2.run".into(), T_WATCH_RUN.to_string());
    let bad = edit(
        &two_watchers,
        "scripts/runs/h.run",
        "watcher = scripts/runs/w.run",
        "watcher = scripts/runs/w2.run",
    );
    triple_only(&bad, &[Rule::NetPairing]);
    let p = triple_problems(&bad);
    assert!(has(&p, "watch", "を指していない"), "{p:?}");
    assert!(!p.iter().any(|(who, _)| *who == "host"), "{p:?}");
    let bad = edit(
        &fs,
        "scripts/runs/g.run",
        "second = scripts/runs/h.run",
        "second = scripts/runs/w.run",
    );
    triple_only(&bad, &[Rule::NetPairing, Rule::Role]);
    let bad = edit(
        &fs,
        "scripts/runs/h.run",
        "watcher = scripts/runs/w.run",
        "watcher = scripts/runs/g.run",
    );
    let p = triple_problems(&bad);
    for r in [Rule::NetPairing, Rule::Watch] {
        assert!(p.iter().any(|(_, x)| x.rule == r), "{r:?} が出ない: {p:?}");
    }
    // 2 台の回の指し合いは、自分のパスを知らない `check` では見ない（既存の使い方は変えない）が、`check_at` は見る。
    let g_def = parse(&fs["scripts/runs/g.run"]).unwrap();
    let mut other = fs.clone();
    other.insert(
        "scripts/runs/h.run".into(),
        T_HOST_RUN.replace("second = scripts/runs/g.run", "second = scripts/runs/x.run"),
    );
    let read = |q: &str| other.get(q).cloned();
    assert!(check(&g_def, &read).is_empty(), "path なしでは見ない");
    let p = check_at(Some("scripts/runs/g.run"), &g_def, &read);
    only(&p, Rule::NetPairing);
}

#[test]
fn the_three_copies_are_all_different() {
    let fs = triple();
    for (same, watch_side_sees) in [("game = a", true), ("game = b", false)] {
        let bad = edit(&fs, "scripts/runs/w.run", "game = c", same);
        triple_only(&bad, &[Rule::NetPairing]);
        // ホストの側は、観戦者がホストとゲストのどちらと同じでも見る。観戦者の側が見るのはホストと同じときだけ（ゲストは知らない）。
        let p = triple_problems(&bad);
        assert!(has(&p, "host", "ゲームのコピー"), "{same}: {p:?}");
        assert_eq!(
            has(&p, "watch", "ゲームのコピー"),
            watch_side_sees,
            "{same}: {p:?}"
        );
    }
}

#[test]
fn only_the_host_has_a_watcher() {
    let fs = triple();
    let none = edit(&fs, "scripts/runs/h.run", "role = host\n", "");
    triple_only(&none, &[Rule::Role, Rule::NetPairing]);
    let swapped = edit(&fs, "scripts/runs/h.run", "role = host", "role = guest");
    let swapped = edit(
        &swapped,
        "scripts/runs/g.run",
        "role = guest",
        "role = host",
    );
    triple_only(&swapped, &[Rule::Role, Rule::NetPairing]);
}

#[test]
fn only_the_host_opens_the_spectate_mouth() {
    let fs = triple();
    for host_ini in [
        edit(
            &fs,
            "scripts/h.ini",
            "[spectate]\nenable = 1\ncode_dir = spec_host\n",
            "",
        ),
        edit(
            &fs,
            "scripts/h.ini",
            "[spectate]\nenable = 1",
            "[spectate]\nenable = 0",
        ),
    ] {
        triple_only(&host_ini, &[Rule::Watch]);
    }
    let guest = edit(
        &fs,
        "scripts/g.ini",
        "host_seat = 1\n",
        "host_seat = 1\n[spectate]\nenable = 1\ncode_dir = spec_guest\n",
    );
    triple_only(&guest, &[Rule::Watch]);
    let p = triple_problems(&guest);
    assert!(has(&p, "guest", "watcher が無い"), "{p:?}");
    assert!(has(&p, "host", "ゲストの ini に [spectate]"), "{p:?}");
    let no_side = edit(&fs, "scripts/w.ini", "code_side = spectate\n", "");
    triple_only(&no_side, &[Rule::Watch]);
    let p = triple_problems(&no_side);
    assert!(has(&p, "host", "code_side = spectate でない"), "{p:?}");
    assert!(has(&p, "watch", "watching は観戦者の定義"), "{p:?}");
}

#[test]
fn the_code_exchange_form_needs_offer_and_answer() {
    let fs = triple();
    let host = "[netplay]\nenable = 1\nscope = lan\nbind = auto:47001\ncode_side = offer\ncode_dir = code_h\nnonce = 0xF000\nhost_seat = 1\n[spectate]\nenable = 1\ncode_dir = spec_host\n";
    let guest = "[netplay]\nenable = 1\nscope = lan\nbind = auto:47002\ncode_side = answer\ncode_dir = code_g\nnonce = 0x0F00\nhost_seat = 1\n";
    let mut ok = fs.clone();
    ok.insert("scripts/h.ini".into(), host.to_string());
    ok.insert("scripts/g.ini".into(), guest.to_string());
    let p = triple_problems(&ok);
    assert!(p.is_empty(), "{p:?}");
    let swapped = |from: &str, to: &str, on: &str| {
        let mut m = ok.clone();
        let t = m[on].replace(from, to);
        assert_ne!(t, m[on], "{on} に {from:?} が無い");
        m.insert(on.to_string(), t);
        m
    };
    for bad in [
        swapped("code_side = offer", "code_side = answer", "scripts/h.ini"),
        swapped("code_side = answer", "code_side = offer", "scripts/g.ini"),
        swapped("code_side = answer\n", "", "scripts/g.ini"),
    ] {
        let p = triple_problems(&bad);
        assert!(has(&p, "host", "code_side = offer"), "{p:?}");
    }
}

#[test]
fn the_code_folders_are_separate() {
    let fs = triple();
    let no_dir = edit(&fs, "scripts/h.ini", "code_dir = spec_host\n", "");
    triple_only(&no_dir, &[Rule::Watch]);
    for same in ["spec_watch", "SPEC_WATCH", "spec_watch/", "spec_watch\\"] {
        let bad = edit(
            &fs,
            "scripts/h.ini",
            "code_dir = spec_host",
            &format!("code_dir = {same}"),
        );
        triple_only(&bad, &[Rule::Watch]);
    }
}

#[test]
fn the_players_agree_on_role_session_and_secret() {
    let fs = triple();
    for nonce in ["nonce = 0x0F00", "nonce = 0x0EFF", "nonce = 0x1"] {
        let bad = edit(&fs, "scripts/h.ini", "nonce = 0xF000", nonce);
        triple_only(&bad, &[Rule::Role]);
    }
    let bad = edit(&fs, "scripts/h.ini", "nonce = 0xF000\n", "");
    triple_only(&bad, &[Rule::Role]);
    let bad = edit(
        &fs,
        "scripts/g.ini",
        "session = 0x48475254",
        "session = 0x1",
    );
    triple_only(&bad, &[Rule::Session]);
    let bad = edit(&fs, "scripts/g.ini", "session = 0x48475254\n", "");
    triple_only(&bad, &[Rule::Session]);
    let bad = edit(&fs, "scripts/g.ini", "0f\n", "0e\n");
    triple_only(&bad, &[Rule::Secret]);
    let ok = edit(&fs, "scripts/g.ini", "0a0b0c0d0e0f", "0A0B0C0D0E0F");
    assert!(triple_problems(&ok).is_empty());
}

#[test]
fn the_sockets_and_relay_ports_are_all_different() {
    let fs = triple();
    for taken in ["127.0.0.1:47001", "127.0.0.1:47002"] {
        let bad = edit(&fs, "scripts/w.ini", "127.0.0.1:47301", taken);
        triple_only(&bad, &[Rule::Address]);
    }
    let bad = edit(
        &fs,
        "scripts/g.ini",
        "peer = 127.0.0.1:47001",
        "peer = 127.0.0.1:47009",
    );
    triple_only(&bad, &[Rule::Address]);
    let relay_h = edit(
        &fs,
        "scripts/h.ini",
        "peer = 127.0.0.1:47002",
        "peer = 127.0.0.1:47011",
    );
    for (h_peer, g_peer) in [
        ("127.0.0.1:47011", "127.0.0.1:47011"),
        ("127.0.0.1:47301", "127.0.0.1:47012"),
        ("127.0.0.1:47011", "127.0.0.1:47301"),
        ("127.0.0.1:47011", "127.0.0.1:47001"),
    ] {
        let h = edit(
            &relay_h,
            "scripts/h.ini",
            "peer = 127.0.0.1:47011",
            &format!("peer = {h_peer}"),
        );
        let bad = edit(
            &h,
            "scripts/g.ini",
            "peer = 127.0.0.1:47001",
            &format!("peer = {g_peer}"),
        );
        let p = triple_problems(&bad);
        assert!(
            !p.is_empty() && p.iter().all(|(_, x)| x.rule == Rule::Address),
            "{h_peer} / {g_peer}: {p:?}"
        );
    }
    let bad = edit(
        &fs,
        "scripts/h.ini",
        "bind = 127.0.0.1:47001",
        "bind = auto:47001",
    );
    triple_only(&bad, &[Rule::Address]);
}

#[test]
fn the_watcher_outlasts_the_players_and_presses_esc_only_when_watching() {
    let fs = triple();
    let bad = edit(&fs, "scripts/runs/w.run", "limit_min = 5", "limit_min = 4");
    triple_only(&bad, &[Rule::Limit]);
    let longer = edit(&fs, "scripts/runs/w.run", "limit_min = 5", "limit_min = 9");
    assert!(triple_problems(&longer).is_empty());
    for step in [
        "wait=10 p1=ESC note=leave\n",
        "wait=10 p2=Z+ESC note=leave\n",
        "scene=1 screen=8 sub=1 wait=30 p1=Z note=chara\n",
        "scene=2 wait=600 p1=Z+ESC note=leave_and_z\n",
    ] {
        let mut bad = fs.clone();
        bad.insert("scripts/w.txt".into(), format!("{WATCH_SCRIPT}{step}"));
        triple_only(&bad, &[Rule::Watch]);
    }
    for ok_step in [
        "wait=10 p1=UP+X+Z note=ok\n",
        "scene=2 wait=600 p1=ESC note=watch_quit\n",
        "scene=1 screen=8 sub=1 wait=30 note=wait_only\n",
    ] {
        let mut ok = fs.clone();
        ok.insert("scripts/w.txt".into(), format!("{WATCH_SCRIPT}{ok_step}"));
        assert!(triple_problems(&ok).is_empty(), "{ok_step:?}");
    }
}

#[test]
fn the_watch_keys_only_fit_their_own_shape() {
    let fs = triple();
    for extra in [
        "peer_sim = --bind 127.0.0.1:1\n",
        "second = scripts/runs/g.run\n",
        "role = guest\n",
    ] {
        let mut bad = fs.clone();
        bad.insert("scripts/runs/w.run".into(), format!("{T_WATCH_RUN}{extra}"));
        let p = triple_problems(&bad);
        assert!(
            p.iter()
                .any(|(who, x)| *who == "watch" && x.rule == Rule::Watch),
            "{extra:?}: {p:?}"
        );
    }
    let text = "script = scripts/t.txt\nini = scripts/g.ini\ngame = c\nlimit_min = 5\npurpose = 合成\nwatching = scripts/runs/h.run\n";
    let def = parse(text).unwrap();
    let p = check_at(Some("scripts/runs/x.run"), &def, &|q: &str| {
        fs.get(q).cloned()
    });
    only(&p, Rule::Watch);
    let text = format!("{}watcher = scripts/runs/w.run\n", run_text(None, ""));
    let p = check_with(&text, &files(SCRIPT, OFF_INI, PLAN));
    only(&p, Rule::Watch);
}

#[test]
fn the_m6_4_runs_are_triples_that_differ_from_the_two_game_samples_only_by_the_spectator() {
    let root = repo_root();
    let read = read_repo(&root);
    let mut triples = 0;
    for n in ["1", "2"] {
        let host_path = format!("scripts/runs/m6_4_{n}_host.run");
        let host =
            parse(&read(&host_path).unwrap_or_else(|| panic!("{host_path} が無い"))).unwrap();
        let guest_path = host.second.clone().expect("host に second が無い");
        let watch_path = host.watcher.clone().expect("host に watcher が無い");
        assert_eq!(guest_path, format!("scripts/runs/m6_4_{n}_guest.run"));
        assert_eq!(watch_path, format!("scripts/runs/m6_4_{n}_watch.run"));
        let guest = parse(&read(&guest_path).unwrap()).unwrap();
        let watch = parse(&read(&watch_path).unwrap()).unwrap();
        assert_eq!(
            (host.game, guest.game, watch.game),
            (Game::A, Game::B, Game::C)
        );
        assert_eq!(watch.watching.as_deref(), Some(host_path.as_str()));
        assert_eq!(host.script.as_deref(), Some("scripts/two_02_host.txt"));
        assert_eq!(guest.script.as_deref(), Some("scripts/two_02_guest.txt"));
        // 観戦者を起こさない対照は、同じ定義で観戦用のコードを置かないだけ。ホストの ini は 2 台の見本と観戦のポートの有無だけ違う。
        let (host_sample, guest_sample) = if n == "1" {
            ("scripts/two_host.ini", "scripts/two_guest.ini")
        } else {
            ("scripts/shape_a.ini", "scripts/shape_b.ini")
        };
        assert_eq!(guest.ini, guest_sample, "ゲストの ini は 2 台の見本のまま");
        let mine = higanr_core::ini::parse(&read(&host.ini).unwrap()).unwrap();
        let theirs = higanr_core::ini::parse(&read(host_sample).unwrap()).unwrap();
        let mut compared = 0;
        for section in ["netplay", "replay"] {
            for (k, _) in theirs.keys(section) {
                if section == "netplay" && k == "name" {
                    continue;
                }
                assert_eq!(
                    mine.get(section, k),
                    theirs.get(section, k),
                    "{} の [{section}] {k} が {host_sample} と違う",
                    host.ini
                );
                compared += 1;
            }
        }
        assert!(compared >= 8, "比べた値が {compared} 個しかない");
        assert_eq!(mine.get("spectate", "enable"), Some("1"));
        assert!(
            mine.keys("spectate").len() == 2
                && mine.keys("replay").len() == theirs.keys("replay").len(),
            "ホストの ini が観戦の節以外に足したもの"
        );
        triples += 1;
    }
    assert_eq!(triples, 2, "M6-4 の回が 2 つ揃っていない");
}

const G_PLAYER_SCRIPT: &str = "\
scene=1 screen=1 sub=1 cursor=0 wait=60 limit=3600 p1=DOWN note=title_down1
scene=1 screen=6 sub=1 cursor=4 wait=30 p1=Z note=open_menu
scene=1 screen=6 wait=90 p1=Z note=room
scene=1 screen=6 wait=30 bs=32 type=Synth-Room-1 enter=1 note=room_type
";

const G_GUEST_SCRIPT: &str = "\
scene=1 screen=1 sub=1 cursor=0 wait=60 limit=3600 p1=DOWN note=title_down1
scene=1 screen=6 sub=1 cursor=4 wait=30 p1=Z note=open_menu
scene=1 screen=6 wait=90 p1=Z note=room
scene=1 screen=6 wait=30 bs=32 type=SYNTH-ROOM-1 enter=1 note=room_type
";

const G_WATCH_SCRIPT: &str = "\
scene=1 screen=1 sub=1 cursor=0 wait=60 limit=3600 p1=DOWN note=title_down1
scene=1 screen=6 sub=1 cursor=4 wait=30 p1=Z note=open_menu
scene=1 screen=6 wait=90 p1=DOWN note=to_spectate
scene=1 screen=6 wait=30 p1=Z note=spectate
scene=1 screen=6 wait=30 bs=32 type=synth-room-1 enter=1 note=room_type
scene=1 screen=7 sub=1 wait=60 limit=36000 note=watch_screen7
scene=2 loading=0 wait=180 limit=36000 note=watch_match
";

const G_HOST_INI: &str = "\
[netplay]
enable = 1
bind = 127.0.0.1:47401
code_side = game
clipboard = file
code_dir = code_gh
room_server = https://rooms.example.invalid
nonce = 0xF000
host_seat = 1
[spectate]
enable = 1
";

const G_GUEST_INI: &str = "\
[netplay]
enable = 1
bind = 127.0.0.1:47402
code_side = game
clipboard = file
code_dir = code_gg
room_server = https://rooms.example.invalid
nonce = 0x0F00
host_seat = 1
[spectate]
enable = 1
";

const G_WATCH_INI: &str = "\
[netplay]
enable = 1
bind = 127.0.0.1:47403
code_side = game
clipboard = file
code_dir = code_gw
room_server = https://rooms.example.invalid
";

fn game_triple() -> HashMap<String, String> {
    HashMap::from(
        [
            ("scripts/runs/h.run", T_HOST_RUN),
            ("scripts/runs/g.run", T_GUEST_RUN),
            ("scripts/runs/w.run", T_WATCH_RUN),
            ("scripts/t.txt", G_PLAYER_SCRIPT),
            ("scripts/w.txt", G_WATCH_SCRIPT),
            ("scripts/h.ini", G_HOST_INI),
            ("scripts/g.ini", G_GUEST_INI),
            ("scripts/w.ini", G_WATCH_INI),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string())),
    )
}

fn game_triple_with_guest_script() -> HashMap<String, String> {
    let mut fs = game_triple();
    fs.insert("scripts/gg.txt".into(), G_GUEST_SCRIPT.to_string());
    fs.insert(
        "scripts/runs/g.run".into(),
        T_GUEST_RUN.replace("scripts/t.txt", "scripts/gg.txt"),
    );
    fs
}

#[test]
fn a_good_game_menu_triple_passes() {
    let fs = game_triple_with_guest_script();
    let p = triple_problems(&fs);
    assert!(p.is_empty(), "{p:?}");
    let mut by_ini = fs.clone();
    by_ini.insert(
        "scripts/w.txt".into(),
        G_WATCH_SCRIPT.replace("type=synth-room-1 ", ""),
    );
    by_ini.insert(
        "scripts/w.ini".into(),
        format!("{G_WATCH_INI}room = Synth-Room-1\n"),
    );
    let p = triple_problems(&by_ini);
    assert!(p.is_empty(), "{p:?}");
    let games: Vec<Game> = ["h", "g", "w"]
        .iter()
        .map(|n| parse(&fs[&format!("scripts/runs/{n}.run")]).unwrap().game)
        .collect();
    assert_eq!(games, [Game::A, Game::B, Game::C]);
}

#[test]
fn the_game_menu_triple_meets_in_one_room_on_one_server() {
    let fs = game_triple_with_guest_script();
    for (file, from, to) in [
        (
            "scripts/w.ini",
            "https://rooms.example.invalid",
            "https://other.example.invalid",
        ),
        (
            "scripts/g.ini",
            "room_server = https://rooms.example.invalid\n",
            "",
        ),
        (
            "scripts/w.ini",
            "room_server = https://rooms.example.invalid\n",
            "",
        ),
    ] {
        let bad = edit(&fs, file, from, to);
        let p = triple_problems(&bad);
        assert!(
            !p.is_empty() && p.iter().all(|(_, x)| x.rule == Rule::Watch),
            "{file} {from:?}: {p:?}"
        );
        assert!(has(&p, "host", "room_server"), "{file}: {p:?}");
    }
    for (file, from, to) in [
        ("scripts/w.txt", "type=synth-room-1", "type=synth-room-2"),
        ("scripts/gg.txt", "type=SYNTH-ROOM-1", "type=SYNTH-ROOM-9"),
        ("scripts/t.txt", "type=Synth-Room-1", "type=Another"),
    ] {
        let bad = edit(&fs, file, from, to);
        triple_only(&bad, &[Rule::Watch]);
        let p = triple_problems(&bad);
        assert!(has(&p, "host", "ルーム名"), "{file}: {p:?}");
    }
    let mut none = fs.clone();
    none.insert(
        "scripts/w.txt".into(),
        G_WATCH_SCRIPT.replace("type=synth-room-1 ", ""),
    );
    triple_only(&none, &[Rule::Watch]);
    // 台本が打つ名が ini の room より優先される（ini の room が違っても、台本が揃っていれば通る）。
    let mut ok = fs.clone();
    ok.insert(
        "scripts/w.ini".into(),
        format!("{G_WATCH_INI}room = wrong-start\n"),
    );
    assert!(triple_problems(&ok).is_empty());
}

#[test]
fn the_game_menu_triple_has_separate_sockets_and_code_dirs_and_a_host_nonce() {
    let fs = game_triple_with_guest_script();
    for taken in ["127.0.0.1:47401", "127.0.0.1:47402"] {
        let bad = edit(&fs, "scripts/w.ini", "127.0.0.1:47403", taken);
        triple_only(&bad, &[Rule::Address]);
    }
    let bad = edit(
        &fs,
        "scripts/w.ini",
        "bind = 127.0.0.1:47403",
        "bind = auto:47403",
    );
    triple_only(&bad, &[Rule::Address]);
    for same in ["code_gh", "CODE_GH", "code_gh/", "code_gh\\"] {
        let bad = edit(
            &fs,
            "scripts/w.ini",
            "code_dir = code_gw",
            &format!("code_dir = {same}"),
        );
        triple_only(&bad, &[Rule::Watch]);
    }
    let bad = edit(&fs, "scripts/g.ini", "code_dir = code_gg\n", "");
    let p = triple_problems(&bad);
    assert!(
        !p.is_empty() && p.iter().all(|(_, x)| x.rule == Rule::Watch),
        "{p:?}"
    );
    for nonce in ["nonce = 0x0F00", "nonce = 0x1"] {
        let bad = edit(&fs, "scripts/h.ini", "nonce = 0xF000", nonce);
        triple_only(&bad, &[Rule::Role]);
    }
}

#[test]
fn the_game_menu_triple_uses_the_game_menu_on_all_three_and_only_the_host_opens_the_mouth() {
    let fs = game_triple_with_guest_script();
    for (file, from, to) in [
        (
            "scripts/g.ini",
            "code_side = game\n",
            "code_side = answer\n",
        ),
        (
            "scripts/w.ini",
            "code_side = game\n",
            "code_side = spectate\n",
        ),
    ] {
        let bad = edit(&fs, file, from, to);
        let p = triple_problems(&bad);
        assert!(!p.is_empty(), "{file}: 通った");
        assert!(has(&p, "host", "code_side = game でない"), "{file}: {p:?}");
    }
    // ゲストは `[spectate]` を持ってよい（配る zip の形。上の合成のゲストが持っていて通る）し、持たなくても通る。
    let no_mouth = edit(&fs, "scripts/g.ini", "[spectate]\nenable = 1\n", "");
    let p = triple_problems(&no_mouth);
    assert!(p.is_empty(), "ゲストが持たない形: {p:?}");
    let text = fs["scripts/w.ini"].clone() + "[spectate]\nenable = 1\n";
    let mut bad = fs.clone();
    bad.insert("scripts/w.ini".to_string(), text);
    let p = triple_problems(&bad);
    assert!(
        has(&p, "host", "観戦者の ini に [spectate] enable = 1 がある"),
        "{p:?}"
    );
    assert!(p.iter().all(|(_, x)| x.rule == Rule::Watch), "{p:?}");
    for (from, what) in [
        ("code_dir = code_gw\n", "code_dir"),
        (
            "room_server = https://rooms.example.invalid\n",
            "room_server",
        ),
        ("enable = 1\n", "enable"),
    ] {
        let bad = edit(&fs, "scripts/w.ini", from, "");
        let p = triple_problems(&bad);
        assert!(
            p.iter()
                .any(|(who, x)| *who == "watch" && x.rule != Rule::Missing),
            "{what}: {p:?}"
        );
    }
    let bad = edit(&fs, "scripts/h.ini", "[spectate]\nenable = 1\n", "");
    triple_only(&bad, &[Rule::Watch]);
}

#[test]
fn the_game_menu_watcher_follows_the_same_script_rules_and_needs_no_keep() {
    let fs = game_triple_with_guest_script();
    for step in [
        "scene=1 screen=8 sub=1 wait=30 p1=Z note=chara\n",
        "scene=2 wait=600 p1=Z+ESC note=leave_and_z\n",
    ] {
        let mut bad = fs.clone();
        bad.insert("scripts/w.txt".into(), format!("{G_WATCH_SCRIPT}{step}"));
        triple_only(&bad, &[Rule::Watch]);
    }
    let mut bad = fs.clone();
    bad.insert(
        "scripts/w.txt".into(),
        format!("wait=10 p1=ESC note=leave\n{G_WATCH_SCRIPT}"),
    );
    triple_only(&bad, &[Rule::Watch]);
    let mut ok = fs.clone();
    ok.insert(
        "scripts/w.txt".into(),
        format!("{G_WATCH_SCRIPT}scene=2 wait=600 p1=ESC note=watch_quit\n"),
    );
    assert!(triple_problems(&ok).is_empty());
    for extra in [
        "peer_sim = --bind 127.0.0.1:1\n",
        "second = scripts/runs/g.run\n",
        "role = guest\n",
    ] {
        let mut bad = fs.clone();
        bad.insert("scripts/runs/w.run".into(), format!("{T_WATCH_RUN}{extra}"));
        let p = triple_problems(&bad);
        assert!(
            p.iter()
                .any(|(who, x)| *who == "watch" && x.rule == Rule::Watch),
            "{extra:?}: {p:?}"
        );
    }
    let bad = edit(&fs, "scripts/runs/w.run", "limit_min = 5", "limit_min = 4");
    triple_only(&bad, &[Rule::Limit]);
}

/// rw_1 の台本のメニューの段（`open_menu` の後から `last` まで）を、本物のメニューの状態機械に 1 段ずつ通し、出た指示と最後の状態を返す。
/// 押す段は 1 tick 押して 1 tick 離す（台本の hold=1 と同じ）。ネットワークは閉じたまま・何も待っていない間（Idle）のメニュー。
fn rw_1_menu_walk(script: &str, last: &str) -> (Vec<Cmd>, Menu) {
    let s = higanr_core::script::parse(script).expect("台本が読めない");
    let at = |note: &str| {
        s.steps
            .iter()
            .position(|st| st.note == note)
            .unwrap_or_else(|| panic!("{note} の段が無い"))
    };
    let (from, to) = (at("open_menu"), at(last));
    assert!(from < to, "open_menu が {last} より後");
    let ini = Shape {
        mode: Mode::Rollback,
        delay: 1,
    };
    let mut m = Menu::new(ini)
        .with_room(RoomItem::On(RoomBuf::default()))
        .with_seat(SeatWish::P1);
    m.open(0);
    let mut cmds = Vec::new();
    let mut tick = |m: &mut Menu, word: u16| {
        let o = m.tick(&TickIn {
            word,
            note: Note::default(),
            conn: Conn::Waiting,
            text: TextIn::NONE,
        });
        cmds.extend(o.cmd);
    };
    tick(&mut m, 0);
    for st in &s.steps[from + 1..=to] {
        assert!(
            st.typed.is_none() && st.p2 == 0,
            "メニューの段 {} が文字か p2 を入れる（このなぞりは p1 の押下だけを見る）",
            st.note
        );
        if st.p1 != 0 {
            tick(&mut m, st.p1);
        }
        tick(&mut m, 0);
    }
    (cmds, m)
}

/// rw_1（ランダムマッチと一覧から選ぶ観戦の 3 台）は `watcher` / `watching` で指し合わない（3 台の回の検査はルーム名の一致を要る）ので、
/// 3 台をまたぐ決まりをここで見る: loopback の同じルームサーバー・別の受信ソケットと置き場・ホストの nonce と席・観戦の配る側の ini・
/// 台本のメニューの段が本物のメニューで「ランダムマッチ」と一覧の画面に届くこと・観戦者が行を選んだ後に何も押さないこと。
#[test]
fn rw_1_trio_is_consistent() {
    let root = repo_root();
    let read = read_repo(&root);
    let def = |p: &str| parse(&read(p).unwrap_or_else(|| panic!("{p} が無い"))).unwrap();
    let host = def("scripts/runs/rw_1_host.run");
    let guest = def("scripts/runs/rw_1_guest.run");
    let watch = def("scripts/runs/rw_1_watch.run");
    assert_eq!(host.second.as_deref(), Some("scripts/runs/rw_1_guest.run"));
    assert_eq!(guest.second.as_deref(), Some("scripts/runs/rw_1_host.run"));
    assert_eq!(
        (host.game, guest.game, watch.game),
        (Game::A, Game::B, Game::C)
    );
    assert!(
        watch.second.is_none() && watch.role.is_none() && watch.peer_sim.is_none(),
        "観戦者の定義に対戦の項目がある"
    );
    assert!(
        watch.limit_min >= host.limit_min,
        "観戦者は対戦者より遅れて終わる"
    );

    let ini_of = |d: &RunDef| {
        higanr_core::ini::parse(&read(&d.ini).unwrap_or_else(|| panic!("{} が無い", d.ini)))
            .unwrap()
    };
    let inis = [ini_of(&host), ini_of(&guest), ini_of(&watch)];
    let net = |i: usize, k: &str| inis[i].get("netplay", k).map(str::to_string);
    let server = net(0, "room_server").expect("ホストに room_server が無い");
    assert!(
        server.starts_with("http://127.0.0.1:"),
        "ルームサーバーがこの PC の loopback でない: {server}"
    );
    assert!(
        (1..3).all(|i| net(i, "room_server").as_deref() == Some(server.as_str())),
        "3 台の room_server が同じでない"
    );
    let server_port: u16 = server.rsplit(':').next().unwrap().parse().unwrap();
    let binds: Vec<std::net::SocketAddr> = (0..3)
        .map(|i| {
            net(i, "bind")
                .expect("bind が無い")
                .parse()
                .expect("bind が読めない")
        })
        .collect();
    assert!(binds.iter().all(|b| b.ip().is_loopback()), "{binds:?}");
    assert!(
        binds[0] != binds[1] && binds[0] != binds[2] && binds[1] != binds[2],
        "受信ソケットが重なる: {binds:?}"
    );
    assert!(
        binds.iter().all(|b| b.port() != server_port),
        "ルームサーバーのポート {server_port} が受信ソケットと重なる"
    );
    let dirs: Vec<String> = (0..3)
        .map(|i| {
            net(i, "code_dir")
                .expect("code_dir が無い")
                .to_ascii_lowercase()
        })
        .collect();
    assert!(
        dirs[0] != dirs[1] && dirs[0] != dirs[2] && dirs[1] != dirs[2],
        "置き場が重なる: {dirs:?}"
    );
    let nonce = |i: usize| {
        let v = net(i, "nonce").expect("nonce が無い");
        u64::from_str_radix(v.trim_start_matches("0x"), 16).expect("nonce が 0x の 16 進でない")
    };
    assert!(
        nonce(0) > nonce(1),
        "ホスト（a）の nonce がゲストより大きくない"
    );
    assert_eq!(
        (net(0, "host_seat"), net(1, "host_seat")),
        (Some("1".to_string()), Some("2".to_string())),
        "席の希望がぶつかると、席が待ち列の順で決まる"
    );
    assert_eq!(
        inis[0].get("spectate", "enable"),
        Some("1"),
        "ホストが配る側を起こさない"
    );
    assert_eq!(
        inis[1].get("spectate", "enable"),
        Some("1"),
        "配る ini と同じ形でない"
    );
    assert_eq!(
        inis[2].get("spectate", "enable"),
        None,
        "観戦者が配る側を開く"
    );

    let script = |d: &RunDef| {
        let p = d.script.as_deref().expect("台本が無い");
        read(p).unwrap_or_else(|| panic!("{p} が無い"))
    };
    for (who, d) in [("ホスト", &host), ("ゲスト", &guest)] {
        let (cmds, _) = rw_1_menu_walk(&script(d), "random");
        assert_eq!(
            cmds,
            vec![Cmd::Random],
            "{who}の台本が「ランダムマッチ」だけを決めない"
        );
    }
    let watch_text = script(&watch);
    let (cmds, m) = rw_1_menu_walk(&watch_text, "open_list");
    assert!(
        cmds.is_empty(),
        "観戦者の台本が一覧の前に指示を出す: {cmds:?}"
    );
    assert!(
        m.wants_watch_list(),
        "観戦者の台本が「観戦」の「ランダムマッチ」の一覧に入らない"
    );
    let s = higanr_core::script::parse(&watch_text).unwrap();
    let pick = s
        .steps
        .iter()
        .position(|st| st.note == "pick_row")
        .expect("pick_row の段が無い");
    assert!(s.steps[pick].p1 != 0, "pick_row が押さない（行を選べない）");
    let pressed_after: Vec<&str> = s.steps[pick + 1..]
        .iter()
        .filter(|st| (st.p1 | st.p2) != 0 || st.typed.is_some() || st.keys.is_some())
        .map(|st| st.note.as_str())
        .collect();
    assert!(
        pressed_after.is_empty(),
        "行を選んだ後に押す段がある（選んだ後のメニューは「観戦をやめる」だけ・観戦中は Esc が観戦をやめる）: {pressed_after:?}"
    );
}
