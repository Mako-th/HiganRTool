//! 観戦者の接続コードの見張り（ini の `code_side = spectate`）。
//!
//! 置き場の `peer_code.txt` を `POLL`（250 ms）ごとに見て、中身が前に見たものと違えば観戦用のコード（ホストが `spectate_code.txt` に
//! 書いたもの。種別 3）として読み、読めたら `WatchSlot` に観戦者の設定（session・秘密・ホストの候補）を置く。返しのコードは書かない
//! （観戦者はホストへ JOIN を送るだけ）。`status.txt` にウィンドウの文を書く（ローダーのウィンドウが「状態」に出す）。
//! - 対戦用のコード（種別 1・2）を貼られたら、種別で断って `status.txt` に言う（`CodeError::WrongSide`）。
//! - ホストの候補はネットワークのガード（`guard::check_peer`。ini の `scope`）を通ったものだけを渡す。
//! - ゲームのスレッドはファイルに触らない（`WatchSlot` の原子変数と待たない錠だけ）。繋がった・観戦が終わったら見張りも終わる。
//!   期間（`HANDSHAKE_RETRY_MS` = 30 分）の間に使えるコードが貼られなければ諦める。
#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use higanr_core::code::{self, CodeError};
use higanr_core::handshake::Name;
use higanr_core::spectate::WatcherConfig;

use crate::net_code::{
    PEER_CODE_FILE, STATUS_FILE, Timing, addrs_text, gate, read_peer_file, replace_file,
    status_text, text_of,
};
use crate::net_config::{CodeSide, NetSettings};
use crate::show_text as st;
use crate::spectate_watch::WatchSlot;
use crate::trial_summary::{CodeFacts, Refusal};
use crate::ui_lang;

const NOTES_MAX: usize = 64;

/// 見張りの文。`log` は本ログへ（詳しいまま）、`show` は `status.txt` へ。
struct Line {
    log: String,
    show: String,
}

/// 観戦用のコードのファイルを読めなかった。`e`（日本語の誤りの文）は本ログに必ず残し、ウィンドウには言語に合う文だけを出す。
fn file_error_line(lang: higanr_core::lang::Lang, e: &str) -> Line {
    Line {
        log: e.to_string(),
        show: st::watch_file_unreadable(lang, e),
    }
}

#[derive(Debug)]
struct Shared {
    notes: Mutex<Vec<String>>,
    stop: AtomicBool,
    facts: Mutex<CodeFacts>,
}

impl Shared {
    fn note(&self, line: String) {
        let mut n = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        if n.len() >= NOTES_MAX {
            n.remove(0);
        }
        n.push(line);
    }
}

/// 見張りの持ち手。落とすと止めて待つ。
pub struct WatchCode {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

impl WatchCode {
    pub fn notes(&self) -> Vec<String> {
        self.shared
            .notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn facts(&self) -> CodeFacts {
        self.shared
            .facts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn finished(&self) -> bool {
        self.join.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn halt(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for WatchCode {
    fn drop(&mut self) {
        self.halt();
    }
}

/// 見張りの 1 回の結果（ファイルの読み書きを含まない判断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Same,
    Refused(String, String, Option<Refusal>),
    Ready(Box<WatcherConfigEq>, String, String),
}

/// `WatcherConfig` は `PartialEq` でないので、検査が比べられる形で包む（session・nonce・ホストのアドレスの並び）。
#[derive(Debug, Clone)]
pub struct WatcherConfigEq(pub WatcherConfig);

impl PartialEq for WatcherConfigEq {
    fn eq(&self, o: &Self) -> bool {
        let (a, b) = (&self.0, &o.0);
        a.session_id == b.session_id
            && a.secret.0 == b.secret.0
            && a.nonce == b.nonce
            && a.host.as_slice() == b.host.as_slice()
    }
}

impl Eq for WatcherConfigEq {}

/// 受け渡しの状態（pure。ファイルの読み書きは見張りのスレッド）。
#[derive(Debug, Clone)]
pub struct Reader {
    scope: higanr_core::guard::Scope,
    name: Name,
    nonce: u64,
    seen: Option<Vec<u8>>,
}

impl Reader {
    pub fn new(s: &NetSettings, nonce: u64) -> Reader {
        Reader {
            scope: s.scope,
            name: Name::new(&s.name),
            nonce,
            seen: None,
        }
    }

    pub fn on_peer_file(&mut self, bytes: Option<&[u8]>, now: u64) -> Step {
        // ファイルが消えた（人が消した）だけなら前に見た中身は忘れない（同じ文字を貼り直しても読み直さない）。
        let Some(b) = bytes else {
            return Step::Same;
        };
        if self.seen.as_deref() == Some(b) {
            return Step::Same;
        }
        self.seen = Some(b.to_vec());
        let text = match text_of(b) {
            Ok(t) => t,
            Err(why) => {
                return Step::Refused(
                    format!("{PEER_CODE_FILE} を読めない: {why}"),
                    st::WATCH_UNREADABLE.get(ui_lang::current()).to_string(),
                    Some(Refusal::Unreadable),
                );
            }
        };
        if text.trim().trim_start_matches('\u{FEFF}').trim().is_empty() {
            return Step::Refused(
                format!("{PEER_CODE_FILE} が空（観戦用のコードを貼って保存する）"),
                st::WATCH_EMPTY.get(ui_lang::current()).to_string(),
                None,
            );
        }
        let (c, host_public) = match code::read_spectate(text, now) {
            Ok(s) => (s.code, s.host_public),
            Err(e) => {
                let (log, show, why) = refusal(e);
                return Step::Refused(format!("{PEER_CODE_FILE} を読めない: {log}"), show, why);
            }
        };
        let host = match gate(&c.candidates, self.scope) {
            Ok(h) => h,
            Err(say) => return Step::Refused(say.log, say.show, Some(Refusal::NoRoute)),
        };
        let Some(secret) = c.secret else {
            // 観戦用のコードは必ず秘密を持つ（`code::read_spectate` が確かめる）。来たら作りの誤り。
            return Step::Refused(
                "観戦用のコードに秘密が無い".to_string(),
                st::INTERNAL.get(ui_lang::current()).to_string(),
                Some(Refusal::Internal),
            );
        };
        // 観戦者の使い捨ての鍵: JOIN ごとに OS の乱数で引く。引けなければ繋がない。
        let key_seed = match crate::net_udp::draw_key_seed() {
            Ok(k) => k,
            Err(e) => {
                return Step::Refused(
                    e,
                    st::INTERNAL.get(ui_lang::current()).to_string(),
                    Some(Refusal::Internal),
                );
            }
        };
        let who = String::from_utf8_lossy(c.name.as_bytes()).into_owned();
        let addrs = addrs_text(&host);
        Step::Ready(
            Box::new(WatcherConfigEq(WatcherConfig {
                session_id: c.session_id,
                secret,
                nonce: self.nonce,
                name: self.name,
                host,
                host_public,
                key_seed,
            })),
            format!(
                "{PEER_CODE_FILE} の観戦用のコード（session 0x{:08X}）を読んだ。ホストの候補 {addrs} へ JOIN を送る（返しのコードは書かない）",
                c.session_id
            ),
            st::watch_read(ui_lang::current(), &who),
        )
    }
}

/// 観戦用のコードを読めなかった理由を（本ログの文, ウィンドウの文, 試験のまとめの分類）にする。
fn refusal(e: CodeError) -> (String, String, Option<Refusal>) {
    let lang = ui_lang::current();
    match e {
        CodeError::WrongSide(k @ (1 | 2)) => (
            format!(
                "対戦用のコード（種別 {k}）が貼られた。観戦には観戦用のコード（ホストの spectate_code.txt の中身。種別 3）が要る"
            ),
            st::WATCH_NOT_MATCH_CODE.get(lang).to_string(),
            Some(Refusal::WrongKind),
        ),
        CodeError::WrongSide(k) => (
            format!("知らない種別のコード（{k}）"),
            st::WATCH_BROKEN.get(lang).to_string(),
            Some(Refusal::Broken),
        ),
        // ホストの HiganRTool のバージョンが違う。本ログは日本語、ウィンドウは今の言語。
        CodeError::BadVersion(theirs) => {
            let log =
                higanr_core::net_menu::other_version_text(higanr_core::wire::PROTO_VER, theirs);
            let show = higanr_core::net_menu::other_version_text_in(
                lang,
                higanr_core::wire::PROTO_VER,
                theirs,
            );
            (log, show, Some(Refusal::OtherVersion { theirs }))
        }
        // ホストのビルドが違う。本ログは日本語、ウィンドウは今の言語。
        CodeError::OtherBuild { theirs } => {
            let log =
                higanr_core::net_menu::other_build_text(higanr_core::update::BUILD_NUMBER, theirs);
            let show = higanr_core::net_menu::other_build_text_in(
                lang,
                higanr_core::update::BUILD_NUMBER,
                theirs,
            );
            (log, show, Some(Refusal::OtherBuild { theirs }))
        }
        CodeError::Expired { .. } => (
            "期限切れ（ホストに新しい観戦用のコードを頼む）".to_string(),
            st::watch_expired(lang, code::DEFAULT_TTL_SECS),
            Some(Refusal::Expired),
        ),
        e => (
            format!("文字が壊れている（{e:?}。保存の途中なら、保存し終わったときに読み直す）"),
            st::WATCH_BROKEN.get(lang).to_string(),
            Some(Refusal::Broken),
        ),
    }
}

/// 開く（`script_win::open_net` から。ネットワークを動かす前）: 置き場を作り、見張りのスレッドを起こす。戻りの文は本ログの「ネットワーク: 開いた」の行に足す。
pub fn start(
    s: &NetSettings,
    dir: PathBuf,
    nonce: u64,
    slot: Arc<WatchSlot>,
    timing: Timing,
) -> Result<(WatchCode, String), String> {
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("接続コードの置き場 {} を作れない: {e}", dir.display()))?;
    let shared = Arc::new(Shared {
        notes: Mutex::new(Vec::new()),
        stop: AtomicBool::new(false),
        facts: Mutex::new(CodeFacts::new(CodeSide::Spectate)),
    });
    let note = format!(
        " / 観戦者（見るだけの側 spectate）: {} に観戦用のコードが貼られるのを待つ（返しのコードは書かない）",
        dir.join(PEER_CODE_FILE).display()
    );
    let mut w = Runner {
        reader: Reader::new(s, nonce),
        dir,
        slot,
        shared: Arc::clone(&shared),
        timing,
        t0: Instant::now(),
        last_error: None,
    };
    w.say(
        &Line {
            log: format!("{PEER_CODE_FILE} に観戦用のコードが貼られて保存されるのを待っている"),
            show: st::WATCH_WAITING.get(ui_lang::current()).to_string(),
        },
        Kind::Info,
    );
    let join = std::thread::Builder::new()
        .name("higanr-watch-code".into())
        .spawn(move || {
            // パニックは受けて外へ出さない（スレッドの入口）。受けたら見張りをやめる。
            let _ = catch_unwind(AssertUnwindSafe(|| w.run()));
        })
        .map_err(|e| format!("観戦用のコードの見張りのスレッドを起こせない: {e}"))?;
    Ok((
        WatchCode {
            shared,
            join: Some(join),
        },
        note,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Info,
    PeerRead,
    Refused(Option<Refusal>),
    Connected,
    EndedUnconnected,
    GaveUp,
}

struct Runner {
    reader: Reader,
    dir: PathBuf,
    slot: Arc<WatchSlot>,
    shared: Arc<Shared>,
    timing: Timing,
    t0: Instant,
    last_error: Option<String>,
}

impl Runner {
    fn say(&mut self, s: &Line, kind: Kind) {
        let secs = self.t0.elapsed().as_secs_f32();
        {
            let mut f = self.shared.facts.lock().unwrap_or_else(|e| e.into_inner());
            match kind {
                Kind::Info => {}
                Kind::PeerRead => {
                    f.peer_read_at.get_or_insert(secs);
                }
                Kind::Refused(r) => {
                    f.refused = f.refused.saturating_add(1);
                    if r.is_some() {
                        f.last_refusal = r;
                    }
                }
                Kind::Connected => {
                    f.connected_at.get_or_insert(secs);
                }
                Kind::EndedUnconnected => f.ended_unconnected = true,
                Kind::GaveUp => f.gave_up = true,
            }
        }
        self.shared
            .note(format!("観戦用のコード（{secs:.1} 秒）: {}", s.log));
        let _ = replace_file(
            &self.dir,
            STATUS_FILE,
            &status_text(CodeSide::Spectate, secs, &s.show),
        );
    }

    fn run(&mut self) {
        loop {
            if self.shared.stop.load(Ordering::Relaxed) {
                self.say(
                    &Line {
                        log: "見張りを止めた（ネットワークを開けなかった）".to_string(),
                        show: crate::net_code::show_not_started().to_string(),
                    },
                    Kind::Info,
                );
                return;
            }
            if self.slot.is_joined() {
                self.say(
                    &Line {
                        log: "繋がった（ホストの最初の STREAM を受けた）".to_string(),
                        show: st::WATCH_CONNECTED.get(ui_lang::current()).to_string(),
                    },
                    Kind::Connected,
                );
                return;
            }
            if self.slot.is_done() {
                self.say(
                    &Line {
                        log: "観戦が終わった（繋がらずに終えた）".to_string(),
                        show: st::WATCH_ENDED_UNCONNECTED
                            .get(ui_lang::current())
                            .to_string(),
                    },
                    Kind::EndedUnconnected,
                );
                return;
            }
            if self.slot.generation() == 0 && self.t0.elapsed() >= self.timing.window {
                self.slot.give_up();
                let secs = self.timing.window.as_secs();
                self.say(
                    &Line {
                        log: format!("{secs} 秒の間に使えるコードを読めなかったので、観戦をやめた"),
                        show: st::gave_up(ui_lang::current(), secs),
                    },
                    Kind::GaveUp,
                );
                return;
            }
            self.poll_once();
            std::thread::sleep(self.timing.poll);
        }
    }

    fn poll_once(&mut self) {
        let bytes = match read_peer_file(&self.dir) {
            Ok(b) => {
                self.last_error = None;
                b
            }
            Err(e) => {
                if self.last_error.as_deref() != Some(e.as_str()) {
                    self.say(
                        &file_error_line(ui_lang::current(), &e),
                        Kind::Refused(Some(Refusal::Unreadable)),
                    );
                    self.last_error = Some(e);
                }
                return;
            }
        };
        match self
            .reader
            .on_peer_file(bytes.as_deref(), crate::net_udp::now_unix())
        {
            Step::Same => {}
            Step::Refused(log, show, why) => {
                self.say(&Line { log, show }, Kind::Refused(why));
            }
            Step::Ready(cfg, log, show) => {
                self.slot.offer(cfg.0);
                self.say(&Line { log, show }, Kind::PeerRead);
            }
        }
    }
}

#[cfg(test)]
mod tests;
