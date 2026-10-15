//! 観戦者の接続コードの見張り（`code_side = spectate`）の検査。ファイルは一時ディレクトリだけ。接続コード・秘密・名前・アドレスは合成の値。
use std::net::SocketAddr;
use std::time::Duration;

use higanr_core::auth::Secret;
use higanr_core::candidate::Candidates;
use higanr_core::code::{self, Code, Side};
use higanr_core::guard::Scope;
use higanr_core::handshake::Name;

use super::*;
use crate::net_code::{MY_CODE_FILE, replace_file};

const SECRET: Secret = Secret([0x33; 16]);
const SESSION: u32 = 0x5EC7_1234;
const NOW: u64 = 1_900_000_000;

fn lo(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn settings(scope: &str) -> NetSettings {
    crate::net_config::from_text(&format!(
        "[netplay]\nenable = 1\nscope = {scope}\nbind = 127.0.0.1:0\ncode_side = spectate\nname = viewer\n"
    ))
    .unwrap()
    .unwrap()
}

fn cands(addrs: &[SocketAddr]) -> Candidates {
    Candidates::from_addrs(addrs).unwrap()
}

fn code_of(side: Side, secret: Option<Secret>, addrs: &[SocketAddr], expires: u64) -> Code {
    Code {
        side,
        session_id: SESSION,
        secret,
        name: Name::new("host"),
        nat: higanr_core::nat::NatMark::UNKNOWN,
        candidates: cands(addrs),
        expires,
    }
}

/// 配る側の公開鍵の標本（観戦者の設定にそのまま渡ることを見る）。
const HOST_PUBLIC: [u8; 32] = [0x41; 32];

fn spectate_text(addrs: &[SocketAddr], expires: u64) -> String {
    code::write_spectate(
        &code_of(Side::Spectate, Some(SECRET), addrs, expires),
        &HOST_PUBLIC,
    )
    .unwrap()
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "higanrtool_watch_code_{tag}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn a_spectate_code_becomes_a_watcher_config_once() {
    let mut r = Reader::new(&settings("loopback"), 0xFEED);
    let text = spectate_text(&[lo(47_001)], NOW + 600);
    let Step::Ready(cfg, log, show) = r.on_peer_file(Some(text.as_bytes()), NOW) else {
        panic!("読めない");
    };
    let c = cfg.0;
    assert_eq!((c.session_id, c.secret, c.nonce), (SESSION, SECRET, 0xFEED));
    // 配る側の公開鍵はコードの値、観戦者の種は読むたびに OS の乱数（同じ中身を別の読み手が読んでも種は別）。
    assert_eq!(c.host_public, HOST_PUBLIC);
    let Step::Ready(again, _, _) =
        Reader::new(&settings("loopback"), 0xFEED).on_peer_file(Some(text.as_bytes()), NOW)
    else {
        panic!("同じコードを別の読み手が読めない");
    };
    assert_ne!(
        c.key_seed, again.0.key_seed,
        "同じコードから同じ鍵の種を作った"
    );
    assert_eq!(c.name, Name::new("viewer"));
    assert_eq!(c.host, cands(&[lo(47_001)]));
    // ホストの名前（"host"）は本ログ向けの文に書かない。
    assert!(log.contains("47001") && !log.contains("\"host\""), "{log}");
    assert!(show.contains("host"), "{show}");
    assert_eq!(r.on_peer_file(Some(text.as_bytes()), NOW), Step::Same);
    assert_eq!(r.on_peer_file(None, NOW), Step::Same);
    assert_eq!(r.on_peer_file(Some(text.as_bytes()), NOW), Step::Same);
    let text2 = spectate_text(&[lo(47_002)], NOW + 600);
    let Step::Ready(cfg2, _, _) = r.on_peer_file(Some(text2.as_bytes()), NOW) else {
        panic!("2 つ目が読めない");
    };
    assert_eq!(cfg2.0.host, cands(&[lo(47_002)]));
    assert_ne!(
        c.key_seed, cfg2.0.key_seed,
        "貼り直したコードで同じ鍵の種を使った"
    );
    let old = spectate_text(&[lo(47_003)], NOW - 1);
    let Step::Refused(log, show, why) = r.on_peer_file(Some(old.as_bytes()), NOW) else {
        panic!("期限切れを通した");
    };
    assert_eq!(why, Some(Refusal::Expired), "{log}");
    assert!(show.contains("期限"), "{show}");
}

#[test]
fn versus_codes_are_refused_by_kind() {
    let mut r = Reader::new(&settings("loopback"), 1);
    let initiator = code::write_initiator(&code_of(
        Side::Initiator,
        Some(SECRET),
        &[lo(47_001)],
        NOW + 600,
    ))
    .unwrap();
    let responder = code::write_reply(
        &code_of(Side::Responder, None, &[lo(47_002)], NOW + 600),
        &SECRET,
    )
    .unwrap();
    for (what, text) in [("始める側", &initiator), ("返す側", &responder)] {
        let Step::Refused(log, show, why) = r.on_peer_file(Some(text.as_bytes()), NOW) else {
            panic!("{what}のコードを通した");
        };
        assert_eq!(why, Some(Refusal::WrongKind), "{what}: {log}");
        assert!(
            log.contains("対戦用") && log.contains("観戦用"),
            "{what}: {log}"
        );
        assert!(
            show.contains("対戦用") && show.contains("観戦用"),
            "{what}: {show}"
        );
    }
    let good = spectate_text(&[lo(47_003)], NOW + 600);
    assert!(matches!(
        r.on_peer_file(Some(good.as_bytes()), NOW),
        Step::Ready(..)
    ));
}

#[test]
fn empty_broken_and_out_of_scope_codes_are_refused() {
    let mut r = Reader::new(&settings("loopback"), 1);
    let refused = |r: &mut Reader, b: &[u8]| match r.on_peer_file(Some(b), NOW) {
        Step::Refused(log, show, why) => (log, show, why),
        other => panic!("通した: {other:?}"),
    };
    assert_eq!(refused(&mut r, b"").2, None, "空は数えない");
    assert_eq!(refused(&mut r, b"  \r\n").2, None);
    assert_eq!(
        refused(&mut r, &[0xFF, 0xFE, b'a', 0]).2,
        Some(Refusal::Unreadable)
    );
    assert_eq!(
        refused(&mut r, "こわれた".as_bytes()).2,
        Some(Refusal::Broken),
        "base64 でない文字"
    );
    let text = spectate_text(&[lo(47_001)], NOW + 600);
    let (head, _) = text.split_at(text.len() / 2);
    assert_eq!(
        refused(&mut r, head.as_bytes()).2,
        Some(Refusal::Broken),
        "半分だけ貼った"
    );
    let lan = spectate_text(&[SocketAddr::from(([192, 168, 1, 20], 47_001))], NOW + 600);
    let (log, show, why) = refused(&mut r, lan.as_bytes());
    assert_eq!(why, Some(Refusal::NoRoute), "{log}");
    assert!(show.contains("scope") || show.contains("lan"), "{show}");
    let mut r2 = Reader::new(&settings("lan"), 1);
    assert!(matches!(
        r2.on_peer_file(Some(lan.as_bytes()), NOW),
        Step::Ready(..)
    ));
    let _ = Scope::Loopback;
}

fn fast() -> Timing {
    Timing {
        poll: Duration::from_millis(5),
        window: Duration::from_secs(30),
    }
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = std::time::Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(10), "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_watcher_thread_reads_peer_code_txt_and_never_writes_my_code() {
    let dir = temp_dir("thread");
    let slot = Arc::new(WatchSlot::new());
    let now = crate::net_udp::now_unix();
    let (w, note) = start(
        &settings("loopback"),
        dir.clone(),
        7,
        Arc::clone(&slot),
        fast(),
    )
    .unwrap();
    assert!(note.contains(PEER_CODE_FILE), "{note}");
    let status = || std::fs::read_to_string(dir.join(STATUS_FILE)).unwrap_or_default();
    wait_until("最初の状態が書かれない", || {
        status().contains("見るだけの側")
    });
    let initiator = code::write_initiator(&code_of(
        Side::Initiator,
        Some(SECRET),
        &[lo(47_001)],
        now + 600,
    ))
    .unwrap();
    replace_file(&dir, PEER_CODE_FILE, &initiator).unwrap();
    wait_until(
        "対戦用のコードを断った状態が出ない",
        || status().contains("対戦用"),
    );
    assert_eq!(slot.generation(), 0, "対戦用のコードで設定を置いた");
    assert_eq!(w.facts().last_refusal, Some(Refusal::WrongKind));
    replace_file(
        &dir,
        PEER_CODE_FILE,
        &spectate_text(&[lo(47_001)], now + 600),
    )
    .unwrap();
    wait_until("設定が置かれない", || slot.generation() == 1);
    assert!(w.facts().peer_read_at.is_some());
    assert!(
        !dir.join(MY_CODE_FILE).exists(),
        "観戦者が返しのコードを書いた"
    );
    slot.mark_joined();
    wait_until("見張りが終わらない", || w.finished());
    assert!(status().contains("接続しました"), "{}", status());
    assert!(w.facts().connected_at.is_some());
    assert!(!dir.join(MY_CODE_FILE).exists());
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_watcher_thread_gives_up_when_no_usable_code_comes() {
    let dir = temp_dir("giveup");
    let slot = Arc::new(WatchSlot::new());
    let timing = Timing {
        poll: Duration::from_millis(5),
        window: Duration::from_millis(60),
    };
    let (w, _) = start(
        &settings("loopback"),
        dir.clone(),
        7,
        Arc::clone(&slot),
        timing,
    )
    .unwrap();
    wait_until("諦めない", || slot.gave_up());
    wait_until("見張りが終わらない", || w.finished());
    assert!(w.facts().gave_up);
    let status = std::fs::read_to_string(dir.join(STATUS_FILE)).unwrap();
    assert!(status.contains("待機をやめました"), "{status}");
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_spectate_code_of_another_version_is_refused_by_its_version() {
    const SPECTATE_V0: &str =
        "AANEMyIRANJJawAAAAACYWIAAQIDBAUGBwgJCgsMDQ4PAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBu_oMCs";
    let mut r = Reader::new(&settings("loopback"), 1);
    let want = format!(
        "相手のHiganRToolのバージョンが違います（自分 {}・相手 0）。相手と同じzipのHiganRToolを使ってください。",
        higanr_core::wire::PROTO_VER
    );
    match r.on_peer_file(Some(SPECTATE_V0.as_bytes()), NOW) {
        Step::Refused(log, show, why) => {
            assert!(log.contains(&want), "{log}");
            assert_eq!(show, want);
            assert_eq!(why, Some(Refusal::OtherVersion { theirs: 0 }));
        }
        other => panic!("通した: {other:?}"),
    }
    let good = spectate_text(&[lo(47_003)], NOW + 600);
    assert!(matches!(
        r.on_peer_file(Some(good.as_bytes()), NOW),
        Step::Ready(..)
    ));
}

/// 観戦用のコードを断るウィンドウの文は今の言語で出る。本ログの文と試験のまとめの分類は言語で変わらない。
#[test]
fn the_watch_code_refusals_follow_the_current_language() {
    use higanr_core::lang::{Lang, has_japanese};
    for e in [
        CodeError::WrongSide(1),
        CodeError::WrongSide(9),
        CodeError::BadVersion(8),
        CodeError::OtherBuild { theirs: 5 },
        CodeError::Expired { expires: 1, now: 2 },
        CodeError::BadTag,
        CodeError::BadLength,
    ] {
        ui_lang::set(Lang::Ja);
        let (log_ja, show_ja, why_ja) = refusal(e);
        ui_lang::set(Lang::En);
        let (log_en, show_en, why_en) = refusal(e);
        assert_eq!(log_ja, log_en, "{e:?}");
        assert_eq!(why_ja, why_en, "{e:?}");
        assert!(has_japanese(&show_ja), "{e:?}: {show_ja}");
        assert!(!has_japanese(&show_en), "{e:?}: {show_en}");
        assert_ne!(show_ja, show_en);
    }
    ui_lang::set(Lang::En);
    let (_, show, _) = refusal(CodeError::WrongSide(1));
    assert_eq!(
        show,
        "This is a match code. To watch, paste the watch code from the host."
    );
    ui_lang::set(Lang::Ja);
}

/// ファイルの誤りの文（日本語）は、どの言語でも本ログに残り、英語のウィンドウには出ない。
#[test]
fn a_file_error_stays_in_the_log_and_the_english_window_has_no_japanese() {
    use higanr_core::lang::{Lang, has_japanese};
    let io = "peer_code.txt を開けない: アクセスが拒否されました。 (os error 5)";
    for lang in Lang::ALL {
        let line = file_error_line(lang, io);
        assert_eq!(line.log, io);
        assert_eq!(has_japanese(&line.show), lang == Lang::Ja, "{}", line.show);
        assert_eq!(line.show.contains("os error"), lang == Lang::Ja);
    }
}
