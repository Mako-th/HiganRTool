//! 戻り道・片道の観戦コード・ini の書き戻しの実機の回（`scripts/runs/ret_*.run`）の組の検査と、「メニューで設定を決める回が追跡中の ini を
//! 書き換えない」の検査。台本は本物のメニューの状態機械（`net_menu::Menu`）に通して、出る指示を見る。
//!
//! `runs_consistency.rs` の検査（指し合い・席・nonce・ポートの重なりなど）は全部の `.run` に掛かる。ここは ret_* に固有の決まり
//! （台本どうしの時刻の割り付け・BYE の置き場所・メニューで着く項目）だけを見る。

use std::collections::BTreeMap;
use std::path::PathBuf;

use higanr_core::delay_pick::DelayChoice;
use higanr_core::ini::{self, Ini};
use higanr_core::lang::Lang;
use higanr_core::liveness::SILENCE_LIMIT_MS;
use higanr_core::mode_pick::{Pick, Shape};
use higanr_core::net_menu::{Cmd, Conn, Menu, Note, RoomItem, TextIn, TickIn};
use higanr_core::player_name::NameBuf;
use higanr_core::return_flow::MENU_READ_FRAMES;
use higanr_core::return_obs::PAUSE_FIRST_MOVES;
use higanr_core::room_input::RoomBuf;
use higanr_core::runfile::{self, RunDef};
use higanr_core::script::{self, Script};
use higanr_core::wire::{Mode, SeatWish};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(p: &str) -> String {
    std::fs::read_to_string(root().join(p)).unwrap_or_else(|e| panic!("{p} を読めない: {e}"))
}

fn run_def(name: &str) -> RunDef {
    runfile::parse(&read(&format!("scripts/runs/{name}.run")))
        .unwrap_or_else(|e| panic!("{name}.run: {e}"))
}

fn script_of(def: &RunDef) -> Script {
    let p = def.script.as_deref().expect("台本が無い");
    script::parse(&read(p)).unwrap_or_else(|e| panic!("{p}: {e}"))
}

fn ini_of(def: &RunDef) -> Ini {
    ini::parse(&read(&def.ini)).unwrap_or_else(|e| panic!("{}: {e}", def.ini))
}

fn step<'a>(s: &'a Script, note: &str) -> (usize, &'a script::Step) {
    s.steps
        .iter()
        .enumerate()
        .find(|(_, st)| st.note == note)
        .unwrap_or_else(|| panic!("{note} の段が無い"))
}

fn keyed(st: &script::Step) -> bool {
    (st.p1 | st.p2) != 0 || st.typed.is_some() || st.keys.is_some()
}

/// 段の待ち tick の下限（待つ tick と押す tick の和）。
fn ticks(st: &script::Step) -> u32 {
    st.wait + if (st.p1 | st.p2) != 0 { st.hold } else { 0 }
}

// ---- メニューのなぞり ---------------------------------------------------------

/// 台本の `open_menu` の後の段を、本物のメニューに 1 段ずつ通して出た指示を返す（`last` の段まで。None なら最後まで）。ネットワークは閉じたまま・
/// 何も待っていない間（Idle）のメニュー。押す段は 1 tick 押して 1 tick 離す。文字の段（`type=` `bs=` `enter=`）はその tick に 16 単位ずつ入れる。
/// 初期の項目は ini（`room_server` → ルーム名の項目あり、`host_seat` → サイド、`name`、`mode`、`delay`、`[ui] lang`）から作る。
fn menu_cmds(script: &Script, ini: &Ini, last: Option<&str>) -> Vec<Cmd> {
    let open = script
        .steps
        .iter()
        .position(|s| s.note == "open_menu")
        .expect("open_menu の段が無い");
    let to = last.map_or(script.steps.len() - 1, |n| step(script, n).0);
    let mode = if ini.get("netplay", "mode") == Some("delay") {
        Mode::Delay
    } else {
        Mode::Rollback
    };
    let (delay, auto) = match ini.get("netplay", "delay") {
        Some("auto") | None => (1u8, true),
        Some(v) => (v.parse().unwrap_or(1), false),
    };
    let room = if ini.get("netplay", "room_server").is_some() {
        RoomItem::On(
            RoomBuf::from_name(ini.get("netplay", "room").unwrap_or("")).unwrap_or_default(),
        )
    } else {
        RoomItem::Off
    };
    let seat = match ini.get("netplay", "host_seat") {
        Some("1") => SeatWish::P1,
        Some("2") => SeatWish::P2,
        _ => SeatWish::Any,
    };
    let lang = if ini.get("ui", "lang") == Some("en") {
        Lang::En
    } else {
        Lang::Ja
    };
    let mut m = Menu::new(Shape { mode, delay })
        .with_start_auto(auto)
        .with_room(room)
        .with_seat(seat)
        .with_lang(lang)
        .with_player_name(NameBuf::lossy(
            ini.get("netplay", "name").unwrap_or("HiganRTool"),
        ));
    m.open(0);
    let mut cmds = Vec::new();
    let mut tick = |m: &mut Menu, word: u16, text: TextIn| {
        let o = m.tick(&TickIn {
            word,
            note: Note::default(),
            conn: Conn::Waiting,
            text,
        });
        cmds.extend(o.cmd);
    };
    tick(&mut m, 0, TextIn::NONE);
    for st in &script.steps[open + 1..=to] {
        if let Some(t) = st.typed {
            let mut i = 0;
            while i < t.unit_count() {
                let mut tin = TextIn::NONE;
                for k in 0..tin.units.len() {
                    if let Some(u) = t.unit(i + k) {
                        tin.units[k] = u;
                        tin.len = k + 1;
                    }
                }
                i += tin.units.len();
                tick(&mut m, 0, tin);
            }
        }
        if st.p1 != 0 {
            tick(&mut m, st.p1, TextIn::NONE);
        }
        tick(&mut m, 0, TextIn::NONE);
    }
    cmds
}

/// メニューの指示が ini へ書かせる行（最後の指示の値）と、ini の今の値が違うもの: （`節.キー`, 今の値, 書かれる値。行を消すときは「(消す)」）。
fn rewrites(ini: &Ini, cmds: &[Cmd]) -> BTreeMap<String, (Option<String>, String)> {
    let mut fin: BTreeMap<String, String> = BTreeMap::new();
    for c in cmds {
        match c {
            Cmd::Name(n) => {
                fin.insert("netplay.name".into(), n.as_str().to_string());
            }
            Cmd::RoomSet(r) | Cmd::Room(r) => {
                fin.insert("netplay.room".into(), r.as_str().to_string());
            }
            Cmd::Shape(p, d) => {
                fin.insert(
                    "netplay.mode".into(),
                    match p {
                        Pick::Delay => "delay",
                        Pick::Rollback(_) => "rollback",
                    }
                    .into(),
                );
                fin.insert(
                    "netplay.delay".into(),
                    match d {
                        DelayChoice::Auto => "auto".to_string(),
                        DelayChoice::Fixed(n) => n.to_string(),
                    },
                );
            }
            Cmd::Seat(w) => {
                fin.insert(
                    "netplay.host_seat".into(),
                    match w {
                        SeatWish::P1 => "1".to_string(),
                        SeatWish::P2 => "2".to_string(),
                        SeatWish::Any => "(消す)".to_string(),
                    },
                );
            }
            Cmd::Lang(l) => {
                fin.insert(
                    "ui.lang".into(),
                    if *l == Lang::En { "en" } else { "ja" }.to_string(),
                );
            }
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    for (k, new) in fin {
        let (sec, key) = k.split_once('.').unwrap();
        let old = ini.get(sec, key).map(str::to_string);
        let same = match (&old, new.as_str()) {
            (None, "(消す)") => true,
            (Some(o), n) => o == n,
            (None, _) => false,
        };
        if !same {
            out.insert(k, (old, new));
        }
    }
    out
}

// ---- ret_* の組 -------------------------------------------------------------

const RET1: [&str; 8] = [
    "match",
    "endmenu",
    "chara",
    "map",
    "saveq",
    "confirm",
    "vanish_match",
    "vanish_chara",
];

fn all_other_inis() -> Vec<(String, Ini)> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(root().join("scripts")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "ini") {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            if !name.starts_with("ret_")
                && let Ok(i) = ini::parse(&std::fs::read_to_string(&p).unwrap())
            {
                v.push((name, i));
            }
        }
    }
    v
}

/// 壊れたら落ちる: ret_* の ini の受信ソケットか置き場が、ほかの回の ini・ret_* どうしで重なる（同じ PC で 2 つの回が取り合う）。
#[test]
fn ret_inis_do_not_share_sockets_or_code_folders_with_any_other_run() {
    let others = all_other_inis();
    assert!(others.len() > 20, "比べる相手の ini が少なすぎる");
    let mut mine: Vec<(String, Ini)> = Vec::new();
    for e in std::fs::read_dir(root().join("scripts")).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("ret_") && name.ends_with(".ini") {
            mine.push((
                name,
                ini::parse(&std::fs::read_to_string(&p).unwrap()).unwrap(),
            ));
        }
    }
    assert_eq!(
        mine.len(),
        2 + 2 + 3 + 1,
        "ret_0 の 2・ret_1 の 2・ret_3 の 3・ret_4 の 1"
    );
    let key = |i: &Ini, k: &str| i.get("netplay", k).map(str::to_ascii_lowercase);
    for (n, i) in &mine {
        let (bind, dir) = (key(i, "bind").unwrap(), key(i, "code_dir").unwrap());
        assert!(
            bind.starts_with("127.0.0.1:"),
            "{n}: bind が loopback でない"
        );
        for (m, o) in &others {
            assert_ne!(
                Some(&bind),
                key(o, "bind").as_ref(),
                "{n} と {m} の bind が重なる"
            );
            assert_ne!(
                Some(&dir),
                key(o, "code_dir").as_ref(),
                "{n} と {m} の置き場が重なる"
            );
        }
    }
    // ret_1 の 2 つ（8 本の回で共通）と ret_0 の 2 つ・ret_3 の 3 つ・ret_4 は、受信ソケットも置き場も別々の値。
    for (x, (n, i)) in mine.iter().enumerate() {
        for (m, o) in &mine[x + 1..] {
            for k in ["bind", "code_dir"] {
                assert_ne!(key(i, k), key(o, k), "{n} と {m} の {k} が同じ");
            }
        }
    }
    // ret_4 のルームサーバーは誰もいない loopback で、ほかの ini の bind とも重ならない。
    let ret4 = ini::parse(&read("scripts/ret_4_a.ini")).unwrap();
    let server = ret4.get("netplay", "room_server").unwrap();
    assert!(server.starts_with("http://127.0.0.1:"), "{server}");
    let port = server.rsplit(':').next().unwrap();
    for (n, i) in mine.iter().chain(others.iter()) {
        assert!(
            !i.get("netplay", "bind")
                .unwrap_or("")
                .ends_with(&format!(":{port}")),
            "{n} の bind が ret_4 のルームサーバーのポートと重なる"
        );
    }
}

/// 壊れたら落ちる: ret_0_obs の時刻の割り付け —— ゲストの攻撃（一時停止のメニューの `[2]` に混ざる）が、ホストの一時停止の操作が終わる前に始まる。
#[test]
fn ret_0_obs_guest_holds_its_attack_until_the_hosts_pause_steps_are_done() {
    let host = script_of(&run_def("ret_0_obs_host"));
    let guest = script_of(&run_def("ret_0_obs_guest"));
    let (a, _) = step(&host, "match_start");
    let (b, _) = step(&host, "pause2_closed");
    let host_ticks: u32 = host.steps[a + 1..=b].iter().map(ticks).sum();
    // 再開の確定から閉じるまで 20 フレームを 2 回。
    let need = host_ticks + 2 * 20 + 300;
    let (_, hold) = step(&guest, "hold_off_until_pauses_end");
    assert!(
        hold.wait >= need,
        "ゲストの待ち {} tick が足りない（ホストの操作 {need} tick）",
        hold.wait
    );
    assert!(!keyed(hold));
    // ゲストの攻撃は、待ちの段より後。
    let (h, _) = step(&guest, "hold_off_until_pauses_end");
    let (m0, _) = step(&guest, "m000");
    assert!(h < m0);
    // ホストは一時停止の間だけ上・下を押す（pause=1）。終わりのメニューの上・下は定義が名指しする。
    let def = run_def("ret_0_obs_host");
    let (e, _) = step(&host, "end_wait");
    for st in &host.steps[a + 1..e] {
        if (st.p1 & (higanr_core::input_model::keys::UP | higanr_core::input_model::keys::DOWN))
            != 0
        {
            assert_eq!(
                st.cond.pause,
                Some(true),
                "{}: 一時停止の外で上・下を押す",
                st.note
            );
        }
    }
    let ud: Vec<&str> = host.steps[e..]
        .iter()
        .filter(|s| {
            (s.p1 & (higanr_core::input_model::keys::UP | higanr_core::input_model::keys::DOWN))
                != 0
        })
        .map(|s| s.note.as_str())
        .collect();
    assert_eq!(ud.len(), 6, "{ud:?}");
    for n in ud {
        assert!(
            def.allow_updown.iter().any(|a| a == n),
            "{n} が allow_updown に無い"
        );
    }
}

/// 壊れたら落ちる: 戻り道の回の台本の形 —— 切る側（ゲスト）の台本の最後の段が、切りたい場面の条件でない（別の場面で BYE が出る）、
/// 受ける側（ホスト）が戻り道の間に勝手に押す・最後の段が押す段（台本が終わると本物の機器の語を読む）、到着の段が無い。
#[test]
fn ret_1_scripts_cut_in_the_scene_they_name_and_the_host_waits_for_the_arrival() {
    let cut_scene = |n: &str, st: &script::Step| match n {
        "match" => st.cond.scene == Some(2) && st.cond.pause == Some(false),
        "endmenu" => st.cond.scene == Some(2) && st.cond.menu_flag == Some(true),
        "chara" => st.cond.scene == Some(1) && st.cond.screen == Some(8),
        "map" => st.cond.scene == Some(1) && st.cond.screen == Some(16),
        _ => unreachable!(),
    };
    for n in RET1 {
        let host = script_of(&run_def(&format!("ret_1_{n}_host")));
        let guest = script_of(&run_def(&format!("ret_1_{n}_guest")));
        let last = host.steps.last().unwrap();
        assert!(
            !keyed(last),
            "{n}: ホストの最後の段が押す段（終わると本物の機器の語を読む）"
        );
        assert!(last.wait >= 600, "{n}: 着いた後に 10 秒、様子を見る");
        let arrived = ["ret_arrived", "back_on_six"];
        match n {
            "saveq" => {
                assert!(
                    host.steps
                        .iter()
                        .all(|s| !arrived.contains(&s.note.as_str()))
                );
                step(&host, "title_after_quit");
                step(&host, "stay_title");
            }
            _ => {
                let (a, b) = if n == "confirm" {
                    ("back_on_six", "back_still")
                } else {
                    ("ret_arrived", "ret_still_there")
                };
                let (i, st) = step(&host, a);
                let (j, still) = step(&host, b);
                assert_eq!(j, i + 1, "{n}: 到着の次の段で確かめる");
                // 抜けた側（confirm のホスト）はゲームが画面 6・カーソル 0 へ戻す（合わせ直さない）。
                let cursor = if n == "confirm" { 0 } else { 4 };
                for s in [st, still] {
                    assert_eq!(
                        (s.cond.scene, s.cond.screen, s.cond.cursor, s.cond.sub),
                        (Some(1), Some(6), Some(cursor), Some(1)),
                        "{n}"
                    );
                }
                // 通り過ぎただけを見逃さない: 次の段は 1 tick で条件を見て、外れれば台本が止まる（optional にしない）。
                assert!(still.limit == 1 && !still.optional && !keyed(still), "{n}");
            }
        }
        if ["match", "endmenu", "chara", "map"].contains(&n) {
            let lst = guest.steps.last().unwrap();
            assert_eq!(lst.note, "cut_here", "{n}");
            assert!(
                !keyed(lst) && cut_scene(n, lst),
                "{n}: 切る段が場面の条件を持たない / 押す"
            );
            // 切る段より前に、ゲストは台本を終えない（途中で終わると別の場面で切れる）。
            assert!(guest.steps.iter().filter(|s| s.note == "cut_here").count() == 1);
        }
        if n.starts_with("vanish") {
            // 台本の外から終了させる側は、終わらない待ちで終える。
            let lst = guest.steps.last().unwrap();
            assert_eq!(lst.note, "stay_until_killed");
            assert_eq!(lst.wait, 36000, "{n}: 台本の最長の待ち（10 分）で終わる");
            let (_, esc) = step(&host, "cut_esc");
            assert_eq!(esc.p1, higanr_core::input_model::keys::ESC);
            let (_, win) = step(&host, "kill_window");
            assert!(!keyed(win) && win.wait >= 3600, "{n}: 終了させる窓が短い");
        }
    }
    // 受ける側が戻り道の間に押すのは、一時停止の中の決定・上・決定（match / vanish_match）か、終わりのメニューの決定（endmenu）だけ。
    for (n, notes) in [
        (
            "match",
            vec!["ret_title_decide", "ret_confirm_yes", "ret_confirm_decide"],
        ),
        (
            "vanish_match",
            vec![
                "cut_esc",
                "ret_title_decide",
                "ret_confirm_yes",
                "ret_confirm_decide",
            ],
        ),
        ("endmenu", vec!["ret_save_decide"]),
    ] {
        let host = script_of(&run_def(&format!("ret_1_{n}_host")));
        let (ms, _) = step(&host, "match_start");
        let pressed: Vec<&str> = host.steps[ms + 1..]
            .iter()
            .filter(|s| keyed(s))
            .map(|s| s.note.as_str())
            .collect();
        assert_eq!(pressed, notes, "{n}");
    }
    // 切断の後に一時停止の中で押す段は pause=1 を条件にする（開く前に押して試合のキーにならない）。
    for n in ["match", "vanish_match"] {
        let host = script_of(&run_def(&format!("ret_1_{n}_host")));
        for note in ["ret_title_decide", "ret_confirm_yes", "ret_confirm_decide"] {
            assert_eq!(step(&host, note).1.cond.pause, Some(true), "{n} {note}");
        }
    }
    // 終わりのメニューの切断は、保存の画面を飛ばす印を立てるために自動保存が要る。
    for side in ["a", "b"] {
        let i = ini::parse(&read(&format!("scripts/ret_1_{side}.ini"))).unwrap();
        assert_eq!(i.get("replay", "enable"), Some("1"), "ret_1_{side}.ini");
    }
}

/// 壊れたら落ちる: ret_1_saveq の「上を押す段」が allow_updown に無い、または押す向きが上でない。確かめの枠で抜ける段が画面 7 の条件を持たない。
#[test]
fn ret_1_saveq_and_confirm_press_where_the_comments_say() {
    let def = run_def("ret_1_saveq_host");
    assert_eq!(def.allow_updown, vec!["save_quit_up".to_string()]);
    let s = script_of(&def);
    assert_eq!(
        step(&s, "save_quit_up").1.p1,
        higanr_core::input_model::keys::UP
    );
    let c = script_of(&run_def("ret_1_confirm_host"));
    for n in ["leave_x", "leave_to_yes", "leave_decide"] {
        assert_eq!(step(&c, n).1.cond.screen, Some(7), "{n}");
    }
    assert_eq!(step(&c, "leave_x").1.p1, higanr_core::input_model::keys::X);
}

/// 壊れたら落ちる: ret_3 の観戦者の台本が、本物のメニューで「観戦コード入力」に着かない（項目の数え違い）、着く前に作る・貼るを決める、
/// ホストが Ctrl+C を押す段（copy=1）が試合の中に無い、ホストが観戦の配る側を開かない ini、観戦者がルームサーバーを使う ini。
#[test]
fn ret_3_watcher_reaches_the_spectate_code_item_and_the_host_presses_copy_in_the_match() {
    let watch = run_def("ret_3_watch");
    let s = script_of(&watch);
    let cmds = menu_cmds(&s, &ini_of(&watch), Some("try_01"));
    assert_eq!(
        cmds,
        vec![Cmd::SpectateCode],
        "観戦者の台本の最初の決定が「観戦コード入力」でない"
    );
    // 試しの段は全部同じ項目を決める（成功した後は画面 6 の条件で飛ぶ）。
    for i in 2..=6 {
        let (_, t) = step(&s, &format!("try_{i:02}"));
        assert!(
            t.optional && t.limit == 1 && t.cond.screen == Some(6),
            "try_{i:02}"
        );
    }
    assert_eq!(ini_of(&watch).get("netplay", "room_server"), None);
    assert_eq!(ini_of(&watch).get("netplay", "clipboard"), Some("file"));
    assert_eq!(ini_of(&watch).get("spectate", "enable"), None);

    let host = run_def("ret_3_host");
    let hs = script_of(&host);
    let (ms, _) = step(&hs, "match_start");
    let (ci, copy) = step(&hs, "copy_spectate_code");
    assert!(ci > ms);
    assert_eq!(copy.keys.map(|k| k.copy), Some(true));
    assert_eq!(copy.cond.scene, Some(2));
    assert!(copy.wait >= 300, "試合の頭から数秒、置く");
    assert_eq!(ini_of(&host).get("spectate", "enable"), Some("1"));
    assert_eq!(ini_of(&host).get("netplay", "clipboard"), Some("file"));
    // ゲストはホストより後に終わる（試合の頭からの長さで比べる。接続の長さは台本ごとに違うので全体では比べない）。観戦者は、最後の試しの後に
    // 4 分以上観る（ホストが試合の頭から 160 秒ほどで終わるより後まで残る）。
    let after = |s: &Script, from: &str| -> u32 {
        let (i, _) = step(s, from);
        s.steps[i + 1..].iter().map(ticks).sum()
    };
    let gs = script_of(&run_def("ret_3_guest"));
    assert!(
        after(&gs, "match_start") > after(&hs, "match_start"),
        "ゲストの台本がホストより先に終わる"
    );
    assert!(
        after(&s, "try_06") >= 4 * 60 * 60,
        "観戦者が試しの後に観る長さが短い"
    );
    assert!(run_def("ret_3_watch").limit_min >= host.limit_min);
    // 観戦者の定義は対戦の項目を持たない。
    let w = run_def("ret_3_watch");
    assert!(w.second.is_none() && w.role.is_none() && w.peer_sim.is_none());
}

/// 壊れたら落ちる: ret_4 の台本が変える値と、定義の頭の check の `--expect` が食い違う（回の後の判定が台本と別のものを見る）、
/// 原本の ini が変える値と同じ値を既に持つ（書き換えても行が変わらず、書き戻しを見られない）。
#[test]
fn ret_4_script_changes_exactly_the_lines_its_check_expects() {
    let def = run_def("ret_4_ini");
    let s = script_of(&def);
    let ini = ini_of(&def);
    let cmds = menu_cmds(&s, &ini, None);
    let kinds: Vec<&str> = cmds
        .iter()
        .map(|c| match c {
            Cmd::RoomSet(_) => "room",
            Cmd::Name(_) => "name",
            Cmd::Seat(_) => "seat",
            Cmd::Shape(..) => "shape",
            Cmd::Lang(_) => "lang",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        ["room", "name", "seat", "shape", "shape", "lang"],
        "{cmds:?}"
    );
    let got: BTreeMap<String, String> = rewrites(&ini, &cmds)
        .into_iter()
        .map(|(k, (_, new))| (k, new))
        .collect();
    // 定義の頭の `--expect 節.キー=値` を集める。
    let text = read("scripts/runs/ret_4_ini.run");
    let mut want: BTreeMap<String, String> = BTreeMap::new();
    for tok in text
        .lines()
        .filter(|l| l.trim_start().starts_with('#'))
        .flat_map(|l| l.split_whitespace())
        .collect::<Vec<_>>()
        .windows(2)
    {
        if tok[0] == "--expect" {
            let (k, v) = tok[1].split_once('=').expect("--expect の形");
            assert!(
                want.insert(k.to_string(), v.to_string()).is_none(),
                "{k} が重なる"
            );
        }
    }
    assert_eq!(got, want, "台本が変える行と check の期待が違う");
    assert_eq!(got.len(), 6);
    // 行が無いキーは足されてしまう（ini_line_diff は行数の増減を不合格にする）ので、変える 6 キーは原本に全部ある。
    for k in got.keys() {
        let (sec, key) = k.split_once('.').unwrap();
        assert!(ini.get(sec, key).is_some(), "原本に {k} の行が無い");
    }
}

/// 壊れたら落ちる: サイドの回の台本が、メニューの「設定」の画面で「サイド」だけを替え（初めは「おまかせ」）、入口のメニューへ戻って
/// 作る側は「対戦コード作成」・貼る側は「対戦コード入力」を決める。項目の数え違いがあれば別の項目を決めて最後の指示が合わない。
#[test]
fn seat_scripts_set_the_side_in_the_settings_screen_and_return_to_create_or_paste() {
    let walk = |script: &str, ini: &str, last: &str| {
        let s = script::parse(&read(&format!("scripts/{script}.txt"))).unwrap();
        let ini = ini::parse(&read(&format!("scripts/{ini}.ini"))).unwrap();
        menu_cmds(&s, &ini, Some(last))
    };
    let seats = |sides: &[SeatWish], last: Cmd| {
        let mut v: Vec<Cmd> = sides.iter().map(|w| Cmd::Seat(*w)).collect();
        v.push(last);
        v
    };
    use SeatWish::{Any, P1, P2};
    // 初めは「おまかせ」。← は 1P の側へ、→ は おまかせ の側へ 1 つずつ動く。
    for (name, sides) in [
        ("host_p1", vec![P2, P1]),
        ("host_p2", vec![P2]),
        ("host_any", vec![P2, Any]),
        ("host_def", vec![]),
    ] {
        assert_eq!(
            walk(&format!("seat_{name}"), "seat_a", "create"),
            seats(&sides, Cmd::Create),
            "{name}"
        );
    }
    for (name, sides) in [
        ("guest_p1", vec![P2, P1]),
        ("guest_p2", vec![P2]),
        ("guest_any", vec![P2, Any]),
    ] {
        assert_eq!(
            walk(&format!("seat_{name}"), "seat_b", "paste_01"),
            seats(&sides, Cmd::Paste),
            "{name}"
        );
    }
}

/// 追跡中の ini を書き換える既知の回（走査して見つけたもの）。直したら、この一覧から外す。新しい回が加わったらこの検査が落ちる
/// （ゲーム内のメニューで設定を決める回は、ローダーに渡す ini を build\ の下のコピーにする —— `tools/ini_line_diff.py prepare`）。
/// サイドの回（seat_*）は、メニューの並びが変わる前は台本が「サイド」の行に着かず、この検査に見えていなかった。
const KNOWN_REWRITERS: [&str; 12] = [
    "m4_room_band",
    "readme_shots_1",
    "seat_01_p1_p2_guest",
    "seat_01_p1_p2_host",
    "seat_02_p2_p1_guest",
    "seat_02_p2_p1_host",
    "seat_03_p1_p1_guest",
    "seat_03_p1_p1_host",
    "seat_04_any_p2_guest",
    "seat_07_def_p1_guest",
    "seat_08_any_p1_guest",
    "ui_en_1",
];

/// 壊れたら落ちる: ゲーム内のメニューで設定（名前・ルーム名・サイド・通信方式・ディレイ・言語）を決めて、追跡中の `scripts/*.ini` の行を
/// 書き換える回が、既知の一覧の外に増える。台本を本物のメニューに通した結果で見る（`ini_save` は決めた設定をその場で渡された ini へ書く）。
#[test]
fn only_the_known_runs_rewrite_their_tracked_ini() {
    let mut names: Vec<PathBuf> = std::fs::read_dir(root().join("scripts/runs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "run"))
        .collect();
    names.sort();
    let mut rewriters: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut walked = 0;
    for p in names {
        let name = p.file_stem().unwrap().to_string_lossy().into_owned();
        if name == "ret_4_ini" {
            continue; // 原本を直に渡さない回（コピーを渡す）。
        }
        let def = runfile::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let Some(sp) = def.script.as_deref() else {
            continue;
        };
        let ini = ini_of(&def);
        if ini.get("netplay", "code_side") != Some("game") {
            continue;
        }
        let s = script::parse(&read(sp)).unwrap();
        if !s.steps.iter().any(|st| st.note == "open_menu") {
            continue;
        }
        walked += 1;
        let r = rewrites(&ini, &menu_cmds(&s, &ini, None));
        if !r.is_empty() {
            rewriters.insert(name, r.keys().cloned().collect());
        }
    }
    assert!(walked > 40, "なぞった回が少なすぎる: {walked}");
    let found: Vec<&str> = rewriters.keys().map(String::as_str).collect();
    assert_eq!(
        found, KNOWN_REWRITERS,
        "追跡中の ini を書き換える回の一覧が変わった: {rewriters:?}"
    );
}

/// 壊れたら落ちる: ret_0_obs の ini が rollback に戻る（1 ms ごとの読みが進め直しの途中を拾い、tick の数えの検査が誤って落ちる）、
/// 1 回目の一時停止の台本の押しが、照らす側が求める移りの列（`PAUSE_FIRST_MOVES`）と食い違う（押す順が変わる・開いた直後の下が読み始めの数えより後になる）。
#[test]
fn ret_0_obs_runs_in_delay_mode_and_its_first_pause_presses_match_the_moves_the_checker_wants() {
    use higanr_core::input_model::keys::{DOWN, ESC, UP};
    for n in ["ret_0_obs_host", "ret_0_obs_guest"] {
        let i = ini_of(&run_def(n));
        assert_eq!(i.get("netplay", "mode"), Some("delay"), "{n}");
        assert_eq!(i.get("netplay", "rollback_ahead"), None, "{n}");
    }
    let host = script_of(&run_def("ret_0_obs_host"));
    let (a, _) = step(&host, "pause1_open");
    let (b, _) = step(&host, "pause2_open");
    let pressed: Vec<(&str, u16)> = host.steps[a..b]
        .iter()
        .filter(|s| keyed(s))
        .map(|s| (s.note.as_str(), s.p1))
        .collect();
    assert_eq!(
        pressed,
        [
            ("pause1_open", ESC),
            ("pause1_early_down", DOWN),
            ("pause1_to_title", DOWN),
            ("pause1_to_resume", UP),
            ("pause1_esc_resume", ESC),
        ]
    );
    // 開く（0→1）と、効く 3 回の押し（1→2・2→1・1→3）。
    assert_eq!(PAUSE_FIRST_MOVES.len(), 4);
    let (_, early) = step(&host, "pause1_early_down");
    assert!(
        early.wait + early.hold < MENU_READ_FRAMES,
        "開いた直後の下が読み始めの数え {MENU_READ_FRAMES} に届く前に終わらない"
    );
    // 定義が、照らす道具に 1 回目の移りの列を求めさせる。
    for n in ["ret_0_obs_host", "ret_0_obs_guest"] {
        assert!(
            read(&format!("scripts/runs/{n}.run")).contains("--pause-first"),
            "{n}: --pause-first が見るものに無い"
        );
    }
}

/// 壊れたら落ちる: ret_1_vanish_match の終了させる時刻（`kill_copy.py b --after N`）が、台本の Esc（試合の頭の 93 秒後）と相手の沈黙の時間切れ
/// （60 秒）が作る窓の外に出る。試合の頭 T は kill_copy の数えで約 30 秒（実測 1 回）。
#[test]
fn ret_1_vanish_match_kill_time_lies_between_the_silence_limit_and_the_esc() {
    let s = script_of(&run_def("ret_1_vanish_match_host"));
    let (a, _) = step(&s, "match_start");
    let (b, _) = step(&s, "cut_esc");
    let esc: i64 = s.steps[a..b]
        .iter()
        .map(|st| i64::from(ticks(st)))
        .sum::<i64>()
        / 60;
    assert_eq!(esc, 93);
    let low = esc - i64::from(SILENCE_LIMIT_MS / 1000);
    // 相手を待つ知らせ（`NOTICE_TICKS` = 120 tick）が Esc の前に出る。
    let high = esc - 2;
    let text = read("scripts/runs/ret_1_vanish_match_host.run");
    let afters: Vec<i64> = text
        .split("kill_copy.py b --after ")
        .skip(1)
        .map(|t| {
            t.split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    assert!(afters.len() >= 2, "{afters:?}");
    assert!(afters.iter().all(|a| *a == afters[0]), "{afters:?}");
    // 試合の頭 T（kill_copy の数え。実測 1 回で約 30 秒）。
    let t = 30;
    assert!(
        afters[0] - t > low,
        "終了が T+33 秒より前だと、時間切れが Esc より先に来る"
    );
    assert!(
        afters[0] - t < high,
        "終了が T+91 秒より後だと、相手を待つ知らせが Esc に間に合わない"
    );
}

/// 壊れたら落ちる: ret_1_match が「左上の理由」の撮影の段を持たない（証拠が残らない）、撮影の段が押す・一時停止が開く前に撮る。
#[test]
fn ret_1_match_shoots_the_reason_line_while_the_pause_is_open() {
    let host = script_of(&run_def("ret_1_match_host"));
    let (i, shot) = step(&host, "ret_reason_shot");
    let (j, _) = step(&host, "ret_title_decide");
    assert_eq!(i + 1, j);
    assert!(!keyed(shot) && shot.shot_post && !shot.shot_pre);
    assert_eq!(shot.cond.pause, Some(true));
    assert!(
        shot.limit >= 7200,
        "戻り道が一時停止を開くまで待つ上限が足りない"
    );
}

/// 壊れたら落ちる: ret_1 の 8 本（受信ソケットと置き場を共有する）の起こす順の 0 から、前の回のコピーが残っていないことの確かめ（`kill_copy.py a|b --dry-run`）が抜ける。
/// 「始めた 0 回」を期待する回の見るものから、同じビルドで「始めた 1 回」が出た回と組にして読む注意が抜ける。
#[test]
fn ret_1_host_runs_check_for_leftover_copies_and_pair_the_zero_start_judgment() {
    for n in RET1 {
        let t = read(&format!("scripts/runs/ret_1_{n}_host.run"));
        for c in ["a", "b"] {
            assert!(
                t.contains(&format!("kill_copy.py {c} --dry-run")),
                "{n}: 起こす順の 0 に kill_copy.py {c} --dry-run が無い"
            );
        }
        let zero = ["match", "endmenu", "chara", "map", "saveq", "confirm"].contains(&n);
        assert_eq!(
            t.contains("「始めた 0 回」の判定"),
            zero,
            "{n}: 0 回の判定の注意の有無"
        );
    }
}
