//! メニューで決めた設定（プレイヤー名・ルーム名・同期方式・ディレイ・サイド）を `HiganRTool.ini` の該当の 1 行へ書き戻す（unsafe 無し）。
//!
//! - 設定の原本は ini 1 つ。決まった時点で書くので、ini とメニューはいつも同じ値になり、「どちらが優先」の規則は無い。
//! - 書くのは決めた tick で起こした別のスレッド。ゲームのスレッドはスレッドを 1 本起こすだけで、確保・ファイルの I/O を足さない。
//! - 続けて決めたとき、古い頼みが新しい値を上書きしないよう、頼みごとに番号を振り、書く直前（ini の鍵の中）で最新かを確かめる。
//! - 行の書き換えは `higanr_core::ini_edit`、ファイルの出し入れは `ini_store`。サイドの「おまかせ」は `host_seat` の行を消す
//!   （読み手は行が無ければおまかせ）。
//! - 以前の版が別ファイルに覚えていた名前・ルーム名は、起動のとき `migrate_legacy` が ini へ移して消す。

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use higanr_core::delay_pick::DelayChoice;
use higanr_core::ini_edit::Edit;
use higanr_core::ini_store::{self, Legacy};
use higanr_core::lan_addr::LanAddr;
use higanr_core::mode_pick::Pick;
use higanr_core::player_name::{self, NameBuf};
use higanr_core::room_input::{self, RoomBuf};
use higanr_core::wire::SeatWish;

use crate::net_config::{DEFAULT_NAME, SECTION};

/// 起動のときに読んだ ini のパス（メニューを置くとき 1 回だけ置く）。無ければ（ini なしの実行）書かずに本ログへ出す。
static TARGET: OnceLock<Option<PathBuf>> = OnceLock::new();

pub fn set_target(path: Option<PathBuf>) {
    let _ = TARGET.set(path);
}

/// 頼みの番号。取るのはゲームのスレッド、見るのは書くスレッドの ini の鍵の中。
struct Seq(AtomicU32);

impl Seq {
    const fn new() -> Seq {
        Seq(AtomicU32::new(0))
    }

    fn next(&self) -> u32 {
        self.0.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    }

    fn is_latest(&self, seq: u32) -> bool {
        self.0.load(Ordering::Acquire) == seq
    }
}

static NAME_SEQ: Seq = Seq::new();
static ROOM_SEQ: Seq = Seq::new();
static SHAPE_SEQ: Seq = Seq::new();
static SEAT_SEQ: Seq = Seq::new();
static LAN_SEQ: Seq = Seq::new();

/// 失敗の文には値（名前・ルーム名）を含めない。
fn report(what: &str, r: Result<bool, String>) {
    if let Err(e) = r {
        crate::main_log::push(format!("設定: {what}を ini へ書けなかった（{e}）"));
    }
}

/// 頼みに番号を振り、別のスレッドで書く。`target` が無ければ書かずに本ログへ出す。
fn ask(
    what: &'static str,
    target: Option<PathBuf>,
    seq: &'static Seq,
    work: impl FnOnce(&Path, u32) -> Result<bool, String> + Send + 'static,
) -> Option<std::thread::JoinHandle<()>> {
    let Some(path) = target else {
        crate::main_log::push(format!(
            "設定: {what}を ini へ書けない（ini の置き場が分からない）"
        ));
        return None;
    };
    let seq = seq.next();
    let spawned = std::thread::Builder::new()
        .name("higanr-ini".into())
        .spawn(move || report(what, work(&path, seq)));
    match spawned {
        Ok(h) => Some(h),
        Err(e) => {
            crate::main_log::push(format!("設定: {what}を書くスレッドを起こせなかった（{e}）"));
            None
        }
    }
}

fn target() -> Option<PathBuf> {
    TARGET.get().cloned().flatten()
}

/// プレイヤー名を決めた（ゲームのスレッド）。
pub fn save_name(n: NameBuf) {
    name_job(target(), n);
}

fn name_job(t: Option<PathBuf>, n: NameBuf) -> Option<std::thread::JoinHandle<()>> {
    ask("名前", t, &NAME_SEQ, move |p, seq| {
        write_name(p, &n, || NAME_SEQ.is_latest(seq))
    })
}

/// 設定の「ルーム名」を決めた（ゲームのスレッド）。
pub fn save_room(n: RoomBuf) {
    room_job(target(), n);
}

fn room_job(t: Option<PathBuf>, n: RoomBuf) -> Option<std::thread::JoinHandle<()>> {
    ask("ルーム名", t, &ROOM_SEQ, move |p, seq| {
        write_room(p, &n, || ROOM_SEQ.is_latest(seq))
    })
}

/// 同期方式かディレイを替えた（ゲームのスレッド）。どちらを替えても 2 行とも書く。
pub fn save_shape(pick: Pick, delay: DelayChoice) {
    shape_job(target(), pick, delay);
}

fn shape_job(
    t: Option<PathBuf>,
    pick: Pick,
    delay: DelayChoice,
) -> Option<std::thread::JoinHandle<()>> {
    ask(
        "同期方式とディレイ",
        t,
        &SHAPE_SEQ,
        move |p, seq| write_shape(p, pick, delay, || SHAPE_SEQ.is_latest(seq)),
    )
}

/// サイドを替えた（ゲームのスレッド）。
pub fn save_seat(wish: SeatWish) {
    seat_job(target(), wish);
}

fn seat_job(t: Option<PathBuf>, wish: SeatWish) -> Option<std::thread::JoinHandle<()>> {
    ask("サイド", t, &SEAT_SEQ, move |p, seq| {
        write_seat(p, wish, || SEAT_SEQ.is_latest(seq))
    })
}

/// LAN のホストのアドレスを決めた（ゲームのスレッド）。
pub fn save_lan_host(a: LanAddr) {
    lan_host_job(target(), a);
}

fn lan_host_job(t: Option<PathBuf>, a: LanAddr) -> Option<std::thread::JoinHandle<()>> {
    ask(
        "LAN のホストのアドレス",
        t,
        &LAN_SEQ,
        move |p, seq| write_lan_host(p, a, || LAN_SEQ.is_latest(seq)),
    )
}

/// 以下の `write_*` は、`is_latest` が真のときだけ書く（偽なら `Ok(false)`）。
pub fn write_name(
    path: &Path,
    n: &NameBuf,
    is_latest: impl FnOnce() -> bool,
) -> Result<bool, String> {
    ini_store::update_if(
        path,
        &[Edit::Set {
            section: SECTION,
            key: "name",
            value: n.as_str(),
        }],
        is_latest,
    )
}

/// `lan_host` の行（ポートが既定なら省いた形）。
pub fn write_lan_host(
    path: &Path,
    a: LanAddr,
    is_latest: impl FnOnce() -> bool,
) -> Result<bool, String> {
    ini_store::update_if(
        path,
        &[Edit::Set {
            section: SECTION,
            key: "lan_host",
            value: a.to_buf().as_str(),
        }],
        is_latest,
    )
}

pub fn write_room(
    path: &Path,
    n: &RoomBuf,
    is_latest: impl FnOnce() -> bool,
) -> Result<bool, String> {
    ini_store::update_if(
        path,
        &[Edit::Set {
            section: SECTION,
            key: "room",
            value: n.as_str(),
        }],
        is_latest,
    )
}

/// ルームへ入った名を `room` に書く（見張りのスレッドから。その場で書く）。これより前に頼んだ書き込みは捨てる。
pub fn remember_room(path: &Path, n: &RoomBuf) -> Result<bool, String> {
    ROOM_SEQ.next();
    write_room(path, n, || true)
}

pub fn write_shape(
    path: &Path,
    pick: Pick,
    delay: DelayChoice,
    is_latest: impl FnOnce() -> bool,
) -> Result<bool, String> {
    let mode = match pick {
        Pick::Delay => "delay",
        Pick::Rollback(_) => "rollback",
    };
    let delay = match delay {
        DelayChoice::Auto => "auto".to_string(),
        DelayChoice::Fixed(d) => d.to_string(),
    };
    ini_store::update_if(
        path,
        &[
            Edit::Set {
                section: SECTION,
                key: "mode",
                value: mode,
            },
            Edit::Set {
                section: SECTION,
                key: "delay",
                value: &delay,
            },
        ],
        is_latest,
    )
}

pub fn write_seat(
    path: &Path,
    wish: SeatWish,
    is_latest: impl FnOnce() -> bool,
) -> Result<bool, String> {
    let edit = match wish {
        SeatWish::Any => Edit::Remove {
            section: SECTION,
            key: "host_seat",
        },
        SeatWish::P1 => Edit::Set {
            section: SECTION,
            key: "host_seat",
            value: "1",
        },
        SeatWish::P2 => Edit::Set {
            section: SECTION,
            key: "host_seat",
            value: "2",
        },
    };
    ini_store::update_if(path, &[edit], is_latest)
}

fn legacy_name(text: &str) -> Option<String> {
    player_name::parse_file(text)
}

fn legacy_room(text: &str) -> Option<String> {
    room_input::parse_last(text).map(|r| r.as_str().to_string())
}

/// 配る ini の `name` のまま（利用者がまだ決めていない）なら、名前のファイルの値を移す。
fn name_is_default(v: &str) -> bool {
    v == DEFAULT_NAME
}

fn room_is_default(_v: &str) -> bool {
    false
}

/// 以前の版が置き場（`code_dir`）に覚えていた名前とルーム名を ini へ移す（起動のとき 1 回、ini を読む前に）。
/// 返すのは本ログ用の文。
pub fn migrate_legacy(ini_path: &Path, code_dir: &Path) -> Vec<String> {
    ini_store::migrate(
        ini_path,
        None,
        &[
            Legacy {
                file: code_dir.join(player_name::PLAYER_NAME_FILE),
                section: SECTION,
                key: "name",
                parse: legacy_name,
                is_default: name_is_default,
            },
            Legacy {
                file: code_dir.join(room_input::ROOM_LAST_FILE),
                section: SECTION,
                key: "room",
                parse: legacy_room,
                is_default: room_is_default,
            },
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::wire::Mode;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let d = std::env::temp_dir().join(format!(
                "higanr_ini_save_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }

        fn ini(&self, text: &str) -> PathBuf {
            let p = self.0.join("HiganRTool.ini");
            std::fs::write(&p, text).unwrap();
            p
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const INI: &str = "; 設定\r\n[netplay]\r\nenable = 1\r\ncode_side = game\r\nbind = 127.0.0.1:0\r\nclipboard = file\r\nname = HiganRTool\r\n; room =\r\n; host_seat = 1\r\ndelay = auto\r\n\r\n[pacing]\r\nmode = auto\r\n";

    fn settings(p: &Path) -> crate::net_config::NetSettings {
        crate::net_config::from_text(&std::fs::read_to_string(p).unwrap())
            .unwrap()
            .unwrap()
    }

    fn name(s: &str) -> NameBuf {
        NameBuf::from_name(s).unwrap()
    }

    fn room(s: &str) -> RoomBuf {
        RoomBuf::from_name(s).unwrap()
    }

    #[test]
    fn a_decided_name_lands_in_its_line_and_a_restart_reads_it() {
        let t = Tmp::new("name");
        let p = t.ini(INI);
        assert_eq!(write_name(&p, &name("Alice-2"), || true), Ok(true));
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            INI.replace("name = HiganRTool", "name = Alice-2")
        );
        assert_eq!(settings(&p).name, "Alice-2");
    }

    #[test]
    fn a_decided_room_is_added_after_the_last_key_and_read_back() {
        let t = Tmp::new("room");
        let p = t.ini(&INI.replace(
            "code_side = game",
            "code_side = game\r\nroom_server = https://rooms.example",
        ));
        assert_eq!(write_room(&p, &room("my-room"), || true), Ok(true));
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("delay = auto\r\nroom = my-room\r\n"),
            "{text}"
        );
        assert_eq!(settings(&p).room.unwrap().0, "my-room");
        assert_eq!(remember_room(&p, &room("joined")), Ok(true));
        assert_eq!(settings(&p).room.unwrap().0, "joined");
    }

    /// 壊れたら落ちる: 打った LAN のホストのアドレスが `lan_host` の行に書かれない（既定のポートは省く）、書いた行を次の起動が読めない。
    #[test]
    fn a_typed_lan_host_lands_in_its_line_and_a_restart_reads_it() {
        use higanr_core::lan_addr::LanAddr;
        let t = Tmp::new("lan_host");
        let p = t.ini(INI);
        let a = LanAddr {
            ip: std::net::Ipv4Addr::new(192, 168, 7, 20),
            port: 19000,
        };
        assert_eq!(write_lan_host(&p, a, || true), Ok(true));
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("lan_host = 192.168.7.20:19000\r\n"), "{text}");
        assert_eq!(settings(&p).lan_host, Some(a));
        let b = LanAddr {
            ip: std::net::Ipv4Addr::new(10, 0, 0, 3),
            port: higanr_core::lan_addr::DEFAULT_PORT,
        };
        assert_eq!(write_lan_host(&p, b, || true), Ok(true));
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.matches("lan_host").count(), 1, "行が増えた: {text}");
        assert!(text.contains("lan_host = 10.0.0.3\r\n"), "{text}");
        assert_eq!(settings(&p).lan_host, Some(b));
        // 古い頼みは書かない。
        assert_eq!(write_lan_host(&p, a, || false), Ok(false));
        assert_eq!(settings(&p).lan_host, Some(b));
    }

    #[test]
    fn a_decided_shape_writes_mode_and_delay_and_a_restart_starts_from_them() {
        let t = Tmp::new("shape");
        let p = t.ini(INI);
        let cases = [
            (Pick::Delay, DelayChoice::Fixed(3), Mode::Delay, 3, false),
            (
                Pick::Rollback(1),
                DelayChoice::Auto,
                Mode::Rollback,
                1,
                true,
            ),
            (
                Pick::Rollback(5),
                DelayChoice::Fixed(5),
                Mode::Rollback,
                5,
                false,
            ),
        ];
        for (pick, delay, mode, d, auto) in cases {
            write_shape(&p, pick, delay, || true).unwrap();
            let s = settings(&p);
            assert_eq!(s.mode, mode);
            assert_eq!(s.rollback_delay_auto, auto);
            if !auto {
                assert_eq!(s.rollback_delay, d);
            }
            let (shape, start_auto) = crate::script_win::menu_start(&s);
            assert_eq!(shape.mode, mode);
            assert_eq!(start_auto, auto);
        }
        let text = std::fs::read_to_string(&p).unwrap();
        // [pacing] の mode には触らない。
        assert!(text.contains("[pacing]\r\nmode = auto\r\n"), "{text}");
        assert_eq!(text.matches("mode = ").count(), 2, "{text}");
    }

    #[test]
    fn the_seat_is_written_as_1_or_2_and_any_removes_the_line() {
        let t = Tmp::new("seat");
        let p = t.ini(INI);
        write_seat(&p, SeatWish::P2, || true).unwrap();
        let s = settings(&p);
        assert_eq!(
            crate::script_win::initial_seat(&s),
            SeatWish::P2,
            "{}",
            std::fs::read_to_string(&p).unwrap()
        );
        write_seat(&p, SeatWish::P1, || true).unwrap();
        assert_eq!(crate::script_win::initial_seat(&settings(&p)), SeatWish::P1);
        write_seat(&p, SeatWish::Any, || true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            INI,
            "元の ini に戻らない"
        );
        assert_eq!(
            crate::script_win::initial_seat(&settings(&p)),
            SeatWish::Any
        );
        assert!(settings(&p).ini_notes.is_empty());
    }

    #[test]
    fn a_stale_request_does_not_overwrite_a_newer_one() {
        let t = Tmp::new("stale");
        let p = t.ini(INI);
        let seq = Seq::new();
        let (old, new) = (seq.next(), seq.next());
        assert_eq!(
            write_name(&p, &name("newer"), || seq.is_latest(new)),
            Ok(true)
        );
        assert_eq!(
            write_name(&p, &name("older"), || seq.is_latest(old)),
            Ok(false)
        );
        assert_eq!(settings(&p).name, "newer", "古い頼みが上書きした");
    }

    #[test]
    fn the_savers_write_from_another_thread_and_a_missing_target_writes_nothing() {
        let t = Tmp::new("jobs");
        let p = t.ini(INI);
        name_job(Some(p.clone()), name("Bob"))
            .unwrap()
            .join()
            .unwrap();
        shape_job(Some(p.clone()), Pick::Delay, DelayChoice::Fixed(4))
            .unwrap()
            .join()
            .unwrap();
        seat_job(Some(p.clone()), SeatWish::P1)
            .unwrap()
            .join()
            .unwrap();
        let ini = higanr_core::ini_store::read(&p).unwrap();
        for (k, v) in [
            ("name", "Bob"),
            ("mode", "delay"),
            ("delay", "4"),
            ("host_seat", "1"),
        ] {
            assert_eq!(ini.get("netplay", k), Some(v), "{k}");
        }
        let before = std::fs::read(&p).unwrap();
        assert!(name_job(None, name("x")).is_none());
        assert!(seat_job(None, SeatWish::P2).is_none());
        assert_eq!(std::fs::read(&p).unwrap(), before);
    }

    #[test]
    fn a_failed_write_never_names_the_value() {
        let t = Tmp::new("fail");
        let p = t.ini("[netplay]\r\nname = a\r\nname = b\r\n");
        let err = write_name(&p, &name("secret-name"), || true).unwrap_err();
        assert!(!err.contains("secret-name"), "{err}");
        let missing = t.0.join("none.ini");
        let err = write_room(&missing, &room("secret-room"), || true).unwrap_err();
        assert!(!err.contains("secret-room") && !missing.exists(), "{err}");
    }

    #[test]
    fn legacy_files_move_into_the_ini_and_are_deleted() {
        let t = Tmp::new("legacy");
        let p = t.ini(INI);
        let code = t.0.join("code");
        std::fs::create_dir_all(&code).unwrap();
        std::fs::write(code.join("player_name.txt"), "Old-Name\n").unwrap();
        std::fs::write(code.join("room_last.txt"), "old-room\n").unwrap();
        let logs = migrate_legacy(&p, &code);
        assert_eq!(logs.len(), 2, "{logs:?}");
        assert!(
            logs.iter()
                .all(|l| !l.contains("Old-Name") && !l.contains("old-room"))
        );
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(
            s.contains("name = Old-Name\r\n") && s.contains("room = old-room\r\n"),
            "{s}"
        );
        assert!(!code.join("player_name.txt").exists() && !code.join("room_last.txt").exists());
        assert!(migrate_legacy(&p, &code).is_empty(), "2 回目は何もしない");
    }

    /// 壊れたら落ちる: 配る ini で `;` を外した行（ルーム名・LAN のホスト・同期方式）にメニューの値が書かれず、行が増える。
    #[test]
    fn the_menu_values_land_in_the_shipped_lines_without_adding_lines() {
        let t = Tmp::new("dist");
        let dist = higanr_core::ini_store::dist_ini_text();
        let p = t.ini(&dist);
        let lines = |p: &Path| std::fs::read_to_string(p).unwrap().lines().count();
        let n = lines(&p);
        write_room(&p, &room("dist-room"), || true).unwrap();
        let a = LanAddr {
            ip: std::net::Ipv4Addr::new(192, 168, 7, 20),
            port: higanr_core::lan_addr::DEFAULT_PORT,
        };
        write_lan_host(&p, a, || true).unwrap();
        write_shape(&p, Pick::Delay, DelayChoice::Fixed(2), || true).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        for want in [
            "\r\nroom = dist-room\r\n",
            "\r\nlan_host = 192.168.7.20\r\n",
            "\r\nmode = delay\r\n",
            "\r\ndelay = 2\r\n",
        ] {
            assert!(text.contains(want), "{want:?}");
        }
        assert_eq!(lines(&p), n, "行が増えた");
        let s = settings(&p);
        assert_eq!(s.room.unwrap().0, "dist-room");
        assert_eq!(s.lan_host, Some(a));
        assert_eq!(s.mode, Mode::Delay);
        assert!(s.ini_notes.is_empty());
        // 「おまかせ」は host_seat の行を足して消す。元の行数に戻る。
        write_seat(&p, SeatWish::P2, || true).unwrap();
        assert_eq!(lines(&p), n + 1);
        write_seat(&p, SeatWish::Any, || true).unwrap();
        assert_eq!(lines(&p), n);
    }

    #[test]
    fn a_value_set_in_the_ini_wins_over_the_old_file() {
        let t = Tmp::new("legacy_prefer");
        let code = t.0.join("code");
        std::fs::create_dir_all(&code).unwrap();
        let p = t.ini(&INI.replace(
            "name = HiganRTool",
            "name = Mine\r\nroom_server = https://r.example\r\nroom = ini-room",
        ));
        std::fs::write(code.join("player_name.txt"), "Old-Name\n").unwrap();
        std::fs::write(code.join("room_last.txt"), "old-room\n").unwrap();
        migrate_legacy(&p, &code);
        let s = settings(&p);
        assert_eq!(
            (s.name.as_str(), s.room.unwrap().0.as_str()),
            ("Mine", "ini-room")
        );
        assert!(!code.join("player_name.txt").exists() && !code.join("room_last.txt").exists());
    }

    #[test]
    fn legacy_files_stay_when_they_cannot_be_moved() {
        let t = Tmp::new("legacy_keep");
        let code = t.0.join("code");
        std::fs::create_dir_all(&code).unwrap();
        std::fs::write(code.join("player_name.txt"), "Old-Name\n").unwrap();
        let logs = migrate_legacy(&t.0.join("none.ini"), &code);
        assert_eq!(logs.len(), 1);
        assert!(code.join("player_name.txt").exists());
        std::fs::write(code.join("room_last.txt"), "has space\n").unwrap();
        let p = t.ini(INI);
        migrate_legacy(&p, &code);
        assert!(
            code.join("room_last.txt").exists(),
            "使えない中身のファイルを消した"
        );
        assert!(!code.join("player_name.txt").exists());
    }
}
