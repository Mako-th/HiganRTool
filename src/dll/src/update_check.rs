//! 遊んでいる間の新しいバージョンの確認（unsafe無し）。
//! ゲームのスレッドは`ask`で確認を依頼し、`found`で結果を読むだけ（どちらもアトミック変数）。
//! ハッシュ一覧の取得と検証は専用のスレッドが行う（`start`が作る。ローダーが知らせを許した起動だけ）。
//! いつ取りに行くかとハッシュ一覧が新しいかの判断は`higanr_core::update_notice`。通信は`higanr_http_win`。
//! 署名が合わない・取れない・遅いときは何も知らせずログに1行だけ書く。

#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::Thread;
use std::time::Duration;

use higanr_core::lang::Lang;
use higanr_core::update::{self, MAX_MANIFEST_BYTES};
use higanr_core::update_notice::{self, Found, Skip};
use higanr_http_win::{Proxy, Timeouts};

/// 操作ごとの待ちの上限。専用のスレッドの待ちなのでゲームは止まらないが数秒で諦める。
const TIMEOUTS: Timeouts = Timeouts {
    resolve_ms: 3000,
    connect_ms: 3000,
    send_ms: 3000,
    receive_ms: 3000,
};
/// 取得全体の上限。名前の解決やプロキシの自動探索は操作ごとの上限だけでは収まらないので時間で見切る。
const FETCH_BUDGET: Duration = Duration::from_secs(10);
/// 専用のスレッドのスタック。待って署名を検証するだけ（通信は別のスレッド）。
const WORKER_STACK: usize = 256 * 1024;

/// 見つかった新しいバージョン（`Found::to_raw`。0は見つかっていない）。書き手は専用のスレッド、読み手はゲームのスレッド。
static FOUND: AtomicU32 = AtomicU32::new(0);
/// 確認の依頼（`ask`が立て、専用のスレッドが取り下げる）。
static WANT: AtomicBool = AtomicBool::new(false);
/// 専用のスレッド。`start`が作る。作れなかったら`None`（確認はしない）。
static WORKER: OnceLock<Option<Thread>> = OnceLock::new();

/// 見つかった新しいバージョンを読む（`Found::from_raw`で戻す。ゲームのスレッドで呼んでよい）。
pub fn found() -> u32 {
    FOUND.load(Ordering::Acquire)
}

/// 専用のスレッドを作る。ローダーが知らせを許した起動でだけ呼ぶ。ゲームのスレッドが動く前に読み取りのスレッドから1回呼ぶ。
pub fn start() {
    // 検査のビルドは本物のサーバーへ出ないようスレッドを作らない。
    if cfg!(test) {
        return;
    }
    WORKER.get_or_init(spawn_worker);
}

/// 確認を依頼する（ゲームのスレッド）。フラグを立てて起こすだけ。`start`が済んでいなければ何もしない（確保も待ちも無い）。
pub fn ask() {
    WANT.store(true, Ordering::Release);
    if let Some(Some(t)) = WORKER.get() {
        t.unpark();
    }
}

/// 専用のスレッドの本体。`park`がfalseを返すまで、依頼があるたびに`run`を1回行う。
/// `run`のパニックは受けて次の依頼でもう一度試す。
/// 依頼は`swap`で取り下げてから行う（行っている間に来た依頼を失わない）。
fn worker_loop(want: &AtomicBool, run: &mut dyn FnMut(), park: &mut dyn FnMut() -> bool) {
    while park() {
        if want.swap(false, Ordering::AcqRel) {
            let _ = catch_unwind(AssertUnwindSafe(&mut *run));
        }
    }
}

fn spawn_worker() -> Option<Thread> {
    let spawned = std::thread::Builder::new()
        .name("higanr-update-notice".into())
        .stack_size(WORKER_STACK)
        .spawn(|| {
            worker_loop(&WANT, &mut run_production, &mut || {
                std::thread::park();
                true
            });
        });
    match spawned {
        Ok(h) => Some(h.thread().clone()),
        Err(e) => {
            crate::main_log::push(format!("新バージョン確認: 知らせの確認の開始不能（{e}）。"));
            None
        }
    }
}

fn run_production() {
    let Some(key) = update::update_public_key() else {
        crate::main_log::push("新バージョン確認: 公開鍵の読み込み不能。");
        return;
    };
    run_once(
        &mut WinFetch::production(),
        update::BUILD_NUMBER,
        &key,
        &FOUND,
    );
}

/// ハッシュ一覧の取得（検査は偽物に替える）。
pub trait Fetch {
    fn manifest(&mut self) -> Result<Vec<u8>, String>;
}

/// 本物の取得（WinHTTP）。`server`・`proxy`・`budget`は検査がloopbackへ向け、待ちを縮めるために持つ。
pub struct WinFetch {
    pub server: String,
    pub proxy: Proxy,
    pub budget: Duration,
}

impl WinFetch {
    pub fn production() -> WinFetch {
        WinFetch {
            server: update::UPDATE_SERVER.to_string(),
            proxy: Proxy::Automatic,
            budget: FETCH_BUDGET,
        }
    }
}

impl Fetch for WinFetch {
    fn manifest(&mut self) -> Result<Vec<u8>, String> {
        let url = update::manifest_url(&self.server);
        let ep = update::parse_url(&url).ok_or_else(|| format!("URL が読めない: {url}"))?;
        let proxy = self.proxy;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("higanr-update-fetch".into())
            .spawn(move || {
                let _ = tx.send(
                    higanr_http_win::get(&ep, proxy, TIMEOUTS, MAX_MANIFEST_BYTES, &mut |_| {})
                        .map_err(|e| e.describe(Lang::Ja)),
                );
            })
            .map_err(|e| format!("通信の開始不能（{e}）"))?;
        match rx.recv_timeout(self.budget) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("{} ミリ秒で応答なし", self.budget.as_millis()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("通信が途中で停止".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    Skipped(Skip),
    /// ハッシュ一覧を取れなかった（通信の失敗・時間切れ）。
    Unreachable(String),
    /// 取れたが署名か形が合わない。
    Rejected(String),
    NoNews,
    Found(Found),
}

/// 確認を1回行う。ゲームを止めないよう呼ぶのは専用のスレッド。ローダーの許しの後の歯止め（開発用のビルド）も含む。
pub fn check(fetch: &mut dyn Fetch, build_number: u32, key: &[u8; 32]) -> Report {
    if let Err(skip) = update_notice::gate(build_number) {
        return Report::Skipped(skip);
    }
    let bytes = match fetch.manifest() {
        Ok(b) => b,
        Err(e) => return Report::Unreachable(e),
    };
    match update_notice::judge(&bytes, key, build_number) {
        Ok(Some(f)) => Report::Found(f),
        Ok(None) => Report::NoNews,
        Err(e) => Report::Rejected(e.to_string()),
    }
}

/// `check`の結果が新しいバージョンなら`out`へ置き、結果をログへ書く。
pub fn run_once(
    fetch: &mut dyn Fetch,
    build_number: u32,
    key: &[u8; 32],
    out: &AtomicU32,
) -> Report {
    let report = check(fetch, build_number, key);
    log(&report);
    if let Report::Found(f) = &report {
        out.store(f.to_raw(), Ordering::Release);
    }
    report
}

fn log(report: &Report) {
    use crate::main_log::{push, push_detail};
    match report {
        Report::Skipped(Skip::DevBuild) => {
            push_detail("新バージョン確認: 未実行（開発用のビルド）。")
        }
        Report::Unreachable(e) => {
            push_detail(format!("新バージョン確認: ハッシュ一覧の取得不能（{e}）。"))
        }
        Report::Rejected(e) => push(format!(
            "新バージョン確認: ハッシュ一覧の検証に失敗しました（{e}）。知らせません。"
        )),
        Report::NoNews => push_detail("新バージョン確認: 新しいバージョンはありません。"),
        Report::Found(Found::Newer(Some(v))) => {
            let (a, b, c) = v.parts();
            push_detail(format!(
                "新バージョン確認: 新しいバージョン v{a}.{b}.{c} があります。"
            ));
        }
        Report::Found(_) => {
            push_detail("新バージョン確認: 新しいバージョンがあります（バージョンは読めません）。")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::update::test_key::{public_key, sample_body, sign, sign_with};
    use higanr_core::update_notice::Version;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const OLD: u32 = 100;
    const NEW: u32 = 200;

    struct FakeFetch {
        answer: Result<Vec<u8>, String>,
        calls: u32,
    }

    impl FakeFetch {
        fn ok(body: &str) -> FakeFetch {
            FakeFetch {
                answer: Ok(body.as_bytes().to_vec()),
                calls: 0,
            }
        }
    }

    impl Fetch for FakeFetch {
        fn manifest(&mut self) -> Result<Vec<u8>, String> {
            self.calls += 1;
            self.answer.clone()
        }
    }

    fn signed(version: u32, display: &str) -> String {
        sign(
            &sample_body(version)
                .replace("display 20261008-abcdef0", &format!("display {display}")),
        )
    }

    #[test]
    fn a_newer_signed_manifest_is_found_and_published_in_one_word() {
        let out = AtomicU32::new(0);
        let mut f = FakeFetch::ok(&signed(NEW, "v1.2.3"));
        let r = run_once(&mut f, OLD, &public_key(), &out);
        let want = Found::Newer(Version::new(1, 2, 3));
        assert_eq!(r, Report::Found(want));
        assert_eq!(Found::from_raw(out.load(Ordering::Acquire)), want);
        assert_eq!(f.calls, 1);
    }

    #[test]
    fn nothing_is_published_for_an_old_same_forged_or_missing_manifest() {
        let key = public_key();
        let forged = sign_with(b"another-seed-another-seed-xxxxxx", &sample_body(NEW));
        let cases: Vec<(&str, FakeFetch)> = vec![
            ("同じバージョン", FakeFetch::ok(&signed(OLD, "v1.2.3"))),
            ("古いバージョン", FakeFetch::ok(&signed(OLD - 1, "v1.2.3"))),
            ("別の鍵の署名", FakeFetch::ok(&forged)),
            ("署名の無い本文", FakeFetch::ok(&sample_body(NEW))),
            (
                "取得の失敗",
                FakeFetch {
                    answer: Err("時間切れ".to_string()),
                    calls: 0,
                },
            ),
        ];
        for (name, mut f) in cases {
            let out = AtomicU32::new(0);
            let r = run_once(&mut f, OLD, &key, &out);
            assert!(!matches!(r, Report::Found(_)), "{name}: {r:?}");
            assert_eq!(out.load(Ordering::Acquire), 0, "{name}");
        }
        let r = check(&mut FakeFetch::ok(&forged), OLD, &key);
        assert!(matches!(r, Report::Rejected(_)), "{r:?}");
        let r = check(
            &mut FakeFetch {
                answer: Err("x".into()),
                calls: 0,
            },
            OLD,
            &key,
        );
        assert_eq!(r, Report::Unreachable("x".into()));
    }

    #[test]
    fn a_dev_build_never_touches_the_network() {
        let key = public_key();
        let mut f = FakeFetch::ok(&signed(NEW, "v1.2.3"));
        assert_eq!(check(&mut f, 0, &key), Report::Skipped(Skip::DevBuild));
        assert_eq!(f.calls, 0, "取りに行った");
        let out = AtomicU32::new(0);
        assert_eq!(
            run_once(&mut f, 0, &key, &out),
            Report::Skipped(Skip::DevBuild)
        );
        assert_eq!((f.calls, out.load(Ordering::Acquire)), (0, 0));
        let r = check(&mut f, OLD, &key);
        assert!(matches!(r, Report::Found(_)), "{r:?}");
        assert_eq!(f.calls, 1);
    }

    /// loopbackの偽のサーバー（1接続だけ答える）。外へは出ない。
    fn serve_once(response: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let join = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("接続");
            let mut req = Vec::new();
            let mut buf = [0u8; 512];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = conn.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
            }
            let _ = conn.write_all(&response);
            String::from_utf8_lossy(&req)
                .lines()
                .next()
                .unwrap_or("")
                .to_string()
        });
        (base, join)
    }

    fn http_ok(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn the_real_fetch_gets_the_manifest_from_a_loopback_server_and_finds_the_news() {
        let (base, join) = serve_once(http_ok(&signed(NEW, "v1.2.3")));
        let mut fetch = loopback_fetch(base);
        let out = AtomicU32::new(0);
        let r = run_once(&mut fetch, OLD, &public_key(), &out);
        assert_eq!(r, Report::Found(Found::Newer(Version::new(1, 2, 3))));
        assert_eq!(join.join().unwrap(), "GET /update/manifest HTTP/1.1");
    }

    #[test]
    fn the_real_fetch_reports_a_server_error_as_unreachable() {
        let (base, join) = serve_once(
            b"HTTP/1.1 500 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        let mut fetch = loopback_fetch(base);
        let out = AtomicU32::new(0);
        let r = run_once(&mut fetch, OLD, &public_key(), &out);
        assert_eq!(r, Report::Unreachable("サーバーの答えが 500".to_string()));
        assert_eq!(out.load(Ordering::Acquire), 0);
        let _ = join.join();
    }

    fn loopback_fetch(server: String) -> WinFetch {
        WinFetch {
            server,
            proxy: Proxy::Direct,
            budget: FETCH_BUDGET,
        }
    }

    #[test]
    fn a_server_that_never_answers_is_given_up_at_the_overall_budget() {
        // 接続を受けて黙るloopbackのサーバー。操作ごとの上限（3秒）より先に全体の上限で切れる。
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (release, held) = mpsc::channel::<()>();
        let join = std::thread::spawn(move || {
            let (_conn, _) = listener.accept().expect("接続");
            let _ = held.recv_timeout(Duration::from_secs(10));
        });
        let mut fetch = loopback_fetch(base);
        fetch.budget = Duration::from_millis(300);
        let began = std::time::Instant::now();
        let r = fetch.manifest();
        let took = began.elapsed();
        assert_eq!(r, Err("300 ミリ秒で応答なし".to_string()));
        assert!(took < Duration::from_millis(2000), "{took:?}");
        let _ = release.send(());
        let _ = join.join();
    }

    #[test]
    fn asking_in_a_test_only_raises_the_flag_and_never_starts_a_thread() {
        // 検査のビルドの`start`はスレッドを作らず、`ask`はネットワークへ出ない。
        WANT.store(false, Ordering::Release);
        start();
        ask();
        assert!(WANT.swap(false, Ordering::AcqRel));
        assert!(WORKER.get().is_none());
    }

    /// `park`が`rounds`回trueを返した後にfalseを返す（ループを終わらせる）。
    fn parks(rounds: u32) -> impl FnMut() -> bool {
        let mut left = rounds;
        move || {
            let more = left > 0;
            left = left.saturating_sub(1);
            more
        }
    }

    #[test]
    fn one_ask_runs_the_check_once_even_if_it_came_before_the_first_park() {
        // 起こす前に立ったフラグは最初のparkの後で取り下げる。3回起きても1回しか走らない。
        let want = AtomicBool::new(true);
        let mut runs = 0u32;
        worker_loop(&want, &mut || runs += 1, &mut parks(3));
        assert_eq!(runs, 1);
        assert!(!want.load(Ordering::Acquire), "旗が残った");
    }

    #[test]
    fn without_an_ask_the_check_never_runs() {
        let want = AtomicBool::new(false);
        let mut runs = 0u32;
        worker_loop(&want, &mut || runs += 1, &mut parks(5));
        assert_eq!(runs, 0);
    }

    #[test]
    fn an_ask_that_comes_while_checking_is_not_lost_and_a_panic_does_not_end_the_loop() {
        let want = AtomicBool::new(true);
        let mut runs = 0u32;
        worker_loop(
            &want,
            &mut || {
                runs += 1;
                match runs {
                    // 確認の最中に次の依頼が来る。
                    1 => want.store(true, Ordering::Release),
                    // 2回目はパニック。受けて次の依頼で続ける。
                    2 => {
                        want.store(true, Ordering::Release);
                        panic!("検査用のパニック");
                    }
                    _ => {}
                }
            },
            &mut parks(6),
        );
        assert_eq!(runs, 3, "最中の頼みを落とした／パニックで止まった");
        assert!(!want.load(Ordering::Acquire));
    }
}
