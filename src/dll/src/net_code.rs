//! 接続コードの受け渡し（ini の `code_side = offer | answer`）。
//!
//! 人が接続コードをファイルで受け渡す。置き場は DLL のフォルダの `code\`（ini の `code_dir` で変えられる。
//! 同じ PC で 2 台を起こすときにゲームごとに分けるため）:
//! - `my_code.txt` … 自分のコード（dll が書く。人がこれを相手へ渡す）。始める側（offer）はネットワークを開くときに、返す側（answer）は
//!   相手の始める側のコードが読めたときに書く。繋がったら消す（始める側のコードは秘密 16 B を含む）。ネットワークが繋がらずに終わったとき・
//!   諦めたときも消す（期限 30 分を過ぎたコードは使えない）。
//! - `peer_code.txt` … 相手のコード（人が貼って保存する。dll は読むだけで、消さない）。
//! - `status.txt` … いまの状態（dll が書き直す。ローダーのウィンドウが「状態」の行に 1 行で出す）。文は 2 本に分ける（`Say`）:
//!   本ログの「ネットワークの外」の行へはファイル名・アドレス・内部の語のままの詳しい文、`status.txt` へは接続コードを受け取った人が読む文
//!   （内部の語を使わず、次にすることを 1 つ）。人はファイルでなくウィンドウで貼るので、`peer_code.txt` の名前はウィンドウに出さない。
//!
//! 見張りのスレッド（`higanr-code`）が `POLL`（250 ms）ごとに `peer_code.txt` を読み、中身が前に見たものと違えば読み直す
//! （更新の時刻ではなく中身で見るので、起動の前に貼っておいたコードも読む。保存の途中の半端なファイルは文字の誤りになるが、
//! 保存し終われば中身が替わるので次の回に読み直す）。読めたら繋がりのセッションの設定を `netplay::LinkSlot` に置く。
//! ゲームのスレッドはファイルに触らない（`LinkSlot` の原子変数と待たない錠だけ）。
//!
//! 取り違え（自分のコードを貼った・2 人とも offer か answer を選んだ）は、文字の誤りと分けてログと `status.txt` に出す
//! （`higanr_core::code::read_peer`）。相手の候補はネットワークのガード（`guard::check_peer`。ini の `scope` の範囲。既定は loopback）を
//! 通したものだけをセッションへ渡す（送る直前のガード `net_udp` の `Outlet::send` と 2 か所）。
//!
//! 繋がったのを見たら、ウィンドウの文を「つながりました。あなたは 1P です。」（席は繋がったセッションが決めた自分の席。`LinkSlot::seat`）に
//! 書き直した後で知らせ（`Notify`。本物は `notify_win::connected` —— ゲームのウィンドウのタスクバーの点滅と音）を 1 回呼んで終わる。
//!
//! ウィンドウで選んだ対戦の形: ローダーのウィンドウで相手のコードを貼る前に `0` `1` `2` `delay` と打つと、ローダーが `mode.txt`
//! （`code_files::MODE_FILE`。対象のゲームの pid つき。形は `higanr_core::mode_pick`）を書く。見張りはセッションの設定を置く直前に
//! これを読み（`ModeCtx::choose`）、pid が自分の実行と合うときだけ、HELLO で名乗る形を ini より先に採って設定と一緒に
//! `LinkSlot::offer_with_mode` で置く。rollback を名乗れるのは今までと同じ条件のときだけ（`NetSettings::mode_choice`）。
//! 繋がった後のウィンドウの文には、AGREE で決まった形をつける（`connected_say`）。
//!
//! 遅延への影響: 繋がる前のファイルの見張りだけで、繋がった後のフレームには 0。始める側は返す側のコードを読むまでセッションを作らない
//! （相手の HELLO は捨てる）ので、繋がるまでの時間は「人がコードを貼るまで ＋ 最大 `POLL` ＋ 1 tick」。
#![forbid(unsafe_code)]

use std::io::{ErrorKind, Read};

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use higanr_core::auth::Secret;
use higanr_core::candidate::Candidates;
use higanr_core::code::{self, Code, CodeError, Mine, PeerError};
use higanr_core::code_files;
use higanr_core::delay_pick::DelayChoice;
use higanr_core::guard::{self, Scope};
use higanr_core::handshake::{self, Name};
use higanr_core::lang::Lang;
use higanr_core::link;
use higanr_core::mode_pick::{self, FileRead, Pick};
use higanr_core::nat::NatMark;
use higanr_core::wire::{Mode, Seat};

use crate::net_config::{CodeSide, ModeChoice, NetSettings};
use crate::net_udp::{self, GameNet};
use crate::net_win::os_random;
use crate::netplay::{HANDSHAKE_RETRY_MS, LinkSlot};
use crate::show_text as st;
use crate::trial_summary::{CodeFacts, Refusal, Stun};
use crate::ui_lang;

// ファイルの名前・置き場・見る間隔・読む上限は、ローダーのウィンドウ（`src/loader/src/code_relay.rs`）と同じでなければならないので
// core の `higanr_core::code_files` に置く。ここは同じ名前で使うための再公開。
pub use higanr_core::code_files::{
    MAX_FILE_BYTES as MAX_PEER_FILE, MODE_FILE, MY_CODE_FILE, PEER_CODE_FILE, POLL, STATUS_FILE,
};
const NOTES_MAX: usize = 64;

/// 置き場（pure）。決め方は core の `code_files::code_dir`。
pub fn code_dir(dll_dir: Option<&Path>, setting: Option<&str>) -> Result<PathBuf, String> {
    code_files::code_dir(dll_dir, setting)
        .ok_or_else(|| "DLL のフォルダが分からないので、接続コードの置き場が決まらない".to_string())
}

/// 見張りの文。`log` は本ログへ（詳しいまま）、`show` は `status.txt` へ（ウィンドウで人が読む）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Say {
    pub log: String,
    pub show: String,
    /// 何があったか（試験のまとめ用）。文を読み直さない。
    pub(crate) kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Info,
    PeerRead,
    Refused(Refusal),
    Connected,
    EndedUnconnected,
    GaveUp,
}

impl Say {
    pub(crate) fn new(log: impl Into<String>, show: impl Into<String>) -> Say {
        Say {
            log: log.into(),
            show: show.into(),
            kind: Kind::Info,
        }
    }

    pub(crate) fn kind(mut self, kind: Kind) -> Say {
        self.kind = kind;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Same,
    Refused(Say),
    /// 読めた: セッションに置く設定、書く自分のコード（返す側だけ）、ログと `status.txt` の文。
    Ready {
        link: Box<link::Config>,
        my_code: Option<String>,
        note: Say,
    },
}

#[derive(Debug, Clone)]
pub struct Swapper {
    side: CodeSide,
    /// ネットワークの範囲（ini の `scope`）。相手のコードの候補をこの範囲でガードに通す。
    scope: Scope,
    name: Name,
    own_cands: Candidates,
    hs: handshake::Config,
    offer: Option<(Code, String)>,
    reply: Option<String>,
    seen: Option<Vec<u8>>,
    peer_name: Option<Name>,
    /// 自分のコードに載せる回線の型の印。
    nat: NatMark,
    /// 最後に読めた相手のコードの回線の型の印。
    peer_nat: Option<NatMark>,
}

impl Swapper {
    /// 始める側。回線の型の印を `UNKNOWN` にした `offer_with_nat`（検査の略記）。
    #[cfg(test)]
    pub fn offer(
        s: &NetSettings,
        role_nonce: u64,
        own_cands: Candidates,
        secret: Secret,
        session_id: u32,
        now: u64,
    ) -> Result<Swapper, String> {
        Swapper::offer_with_nat(
            s,
            role_nonce,
            own_cands,
            NatMark::UNKNOWN,
            secret,
            session_id,
            now,
        )
    }

    /// 始める側。自分のコード（`secret`・`session_id`・自分の候補 `own_cands`・回線の型の印 `nat`。有効期限は `now` から 30 分）を作る。
    pub fn offer_with_nat(
        s: &NetSettings,
        role_nonce: u64,
        own_cands: Candidates,
        nat: NatMark,
        secret: Secret,
        session_id: u32,
        now: u64,
    ) -> Result<Swapper, String> {
        let own = Code {
            side: code::Side::Initiator,
            session_id,
            secret: Some(secret),
            name: Name::new(&s.name),
            nat,
            candidates: own_cands,
            expires: now.saturating_add(code::DEFAULT_TTL_SECS),
        };
        let text =
            code::write_initiator(&own).map_err(|e| format!("接続コードを書けない: {e:?}"))?;
        let mut sw = Swapper::new(CodeSide::Offer, s, role_nonce, own_cands);
        sw.nat = nat;
        sw.offer = Some((own, text));
        Ok(sw)
    }

    /// 返す側。自分のコードは相手の始める側のコードが読めてから作る。
    pub fn answer(s: &NetSettings, role_nonce: u64, own_cands: Candidates) -> Swapper {
        Swapper::new(CodeSide::Answer, s, role_nonce, own_cands)
    }

    /// 返すコードに載せる回線の型の印を決める（返す側。コードを書く前に呼ぶ）。
    pub fn with_nat(mut self, nat: NatMark) -> Swapper {
        self.nat = nat;
        self
    }

    /// 最後に読めた相手のコードの回線の型の印。
    pub fn peer_nat(&self) -> Option<NatMark> {
        self.peer_nat
    }

    fn new(side: CodeSide, s: &NetSettings, role_nonce: u64, own_cands: Candidates) -> Swapper {
        Swapper {
            side,
            scope: s.scope,
            name: Name::new(&s.name),
            own_cands,
            hs: s.handshake_config(0, role_nonce),
            offer: None,
            reply: None,
            seen: None,
            peer_name: None,
            nat: NatMark::UNKNOWN,
            peer_nat: None,
        }
    }

    pub fn side(&self) -> CodeSide {
        self.side
    }

    pub fn own_text(&self) -> Option<&str> {
        match self.side {
            CodeSide::Offer => self.offer.as_ref().map(|(_, t)| t.as_str()),
            CodeSide::Answer => self.reply.as_deref(),
            // 見るだけの側は自分のコードを持たない（`Swapper` は作らない）。`Game` は `Swapper` を作るときに始める側か返す側に決める。
            CodeSide::Spectate | CodeSide::Game => None,
        }
    }

    /// 最後に読めた相手の名前（ゲーム内のメニューが「相手「名前」のコードを読みました」に使う）。
    pub fn peer_name(&self) -> Option<Name> {
        self.peer_name
    }

    /// 前に見た相手の中身を忘れる。ゲーム内のメニューの「貼る」は人が選ぶ操作なので、断った文字をもう一度貼られたら黙らずに
    /// 同じ断りをもう一度言う（ファイルの見張りは同じ中身を読み直さない）。
    pub fn forget_peer_seen(&mut self) {
        self.seen = None;
    }

    /// 相手のファイルの中身（無ければ None）を見る。中身が前に見たものと同じなら何もしない。
    pub fn on_peer_file(&mut self, bytes: Option<&[u8]>, now: u64) -> Step {
        // ファイルが消えた（人が消した）だけなら前に見た中身は忘れない（同じ文字を貼り直しても繋ぎ直さない）。
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
                    Say::new(
                        format!("{PEER_CODE_FILE} を読めない: {why}"),
                        st::PEER_UNREADABLE.get(ui_lang::current()),
                    )
                    .kind(Kind::Refused(Refusal::Unreadable)),
                );
            }
        };
        let read = match (&self.offer, self.side) {
            (Some((own, own_text)), CodeSide::Offer) => code::read_peer(
                text,
                Mine::Offer {
                    own,
                    own_text: own_text.as_str(),
                },
                now,
            ),
            (_, CodeSide::Answer) => code::read_peer(
                text,
                Mine::Answer {
                    own_text: self.reply.as_deref(),
                },
                now,
            ),
            (None, CodeSide::Offer) => Err(PeerError::Code(CodeError::Inconsistent)),
            // 見るだけの側は `Swapper` を作らない（観戦用のコードは `spectate_watch_code.rs` が読む）。
            (_, CodeSide::Spectate | CodeSide::Game) => {
                Err(PeerError::Code(CodeError::Inconsistent))
            }
        };
        let peer = match read {
            Ok(c) => c,
            Err(e) => {
                let (log, show) = peer_error_text(e);
                let say = Say::new(format!("{PEER_CODE_FILE} を読めない: {log}"), show);
                return Step::Refused(match refusal_of(e) {
                    Some(r) => say.kind(Kind::Refused(r)),
                    None => say,
                });
            }
        };
        let peers = match gate(&peer.candidates, self.scope) {
            Ok(p) => p,
            Err(why) => return Step::Refused(why),
        };
        let addrs = addrs_text(&peers);
        self.peer_name = Some(peer.name);
        self.peer_nat = Some(peer.nat);
        let who = String::from_utf8_lossy(peer.name.as_bytes()).into_owned();
        match self.side {
            CodeSide::Offer => {
                let Some((own, _)) = &self.offer else {
                    return Step::Refused(internal("自分のコードが無い"));
                };
                let Some(secret) = own.secret else {
                    return Step::Refused(internal("自分のコードに秘密が無い"));
                };
                let link = match self.link_config(own.session_id, secret, peers) {
                    Ok(l) => l,
                    Err(say) => return Step::Refused(say),
                };
                Step::Ready {
                    link: Box::new(link),
                    my_code: None,
                    note: Say::new(
                        format!(
                            "{PEER_CODE_FILE} の返すコードを読んだ。相手の候補 {addrs} へ繋いでいる（相手の回線の型の印: {}）",
                            peer.nat.text()
                        ),
                        st::reply_read(ui_lang::current(), &who),
                    )
                    .kind(Kind::PeerRead),
                }
            }
            CodeSide::Answer => {
                let Some(secret) = peer.secret else {
                    // 読めた始める側のコードには必ず秘密がある（`code::read_peer` が確かめる）。来たら作りの誤り。
                    return Step::Refused(internal("相手の始める側のコードに秘密が無い"));
                };
                let reply =
                    match code::write_reply_to(&peer, self.name, self.nat, self.own_cands, now) {
                        Ok(t) => t,
                        Err(e) => {
                            return Step::Refused(internal(format!("返すコードを書けない: {e:?}")));
                        }
                    };
                let link = match self.link_config(peer.session_id, secret, peers) {
                    Ok(l) => l,
                    Err(say) => return Step::Refused(say),
                };
                self.reply = Some(reply.clone());
                Step::Ready {
                    link: Box::new(link),
                    my_code: Some(reply),
                    note: Say::new(
                        format!(
                            "{PEER_CODE_FILE} の始める側のコードを読み、返すコードを {MY_CODE_FILE} に書いた（相手へ渡す）。相手の候補 {addrs} へ繋いでいる（相手の回線の型の印: {}）",
                            peer.nat.text()
                        ),
                        st::offer_read(ui_lang::current(), &who),
                    )
                    .kind(Kind::PeerRead),
                }
            }
            // 上の `read` で断っている（ここへは来ない）。
            CodeSide::Spectate | CodeSide::Game => {
                Step::Refused(internal("見るだけの側・ゲーム内の側に Swapper は要らない"))
            }
        }
    }

    /// 繋がりのセッションの設定。使い捨ての鍵の種はここで OS の乱数で引く（セッションの設定を作るたびに新しく）。引けなければ断る
    /// （決まった値の種で繋ぐと、ルームサーバーの持ち主にも読める便になる）。
    fn link_config(
        &self,
        session_id: u32,
        secret: Secret,
        peers: Candidates,
    ) -> Result<link::Config, Say> {
        let key_seed = crate::net_udp::draw_key_seed().map_err(internal)?;
        Ok(link::Config {
            handshake: handshake::Config {
                session_id,
                ..self.hs
            },
            secret,
            peers,
            key_seed,
        })
    }
}

/// ファイルの中身 → 文字。メモ帳の「Unicode」（UTF-16）で保存したものは読まずに直し方を言う（UTF-8 の BOM は `code` が除く）。
pub(crate) fn text_of(b: &[u8]) -> Result<&str, String> {
    if b.starts_with(&[0xFF, 0xFE]) || b.starts_with(&[0xFE, 0xFF]) {
        return Err("UTF-16 で保存されている（UTF-8 で保存し直す）".to_string());
    }
    std::str::from_utf8(b).map_err(|_| "UTF-8 として読めない".to_string())
}

fn offer_waiting(ttl_secs: u64) -> Say {
    let span = code::span_text(ttl_secs);
    Say::new(
        format!(
            "自分の接続コード（始める側）を {MY_CODE_FILE} に書いた。相手へ渡し、相手が返すコードを {PEER_CODE_FILE} に貼って保存する（有効期限 {span}）"
        ),
        st::offer_waiting(ui_lang::current(), ttl_secs),
    )
}

/// 繋がったときの文。`seat` は繋がったセッションが決めた自分の席（`LinkSlot::seat`）。席は画面から読めない値なのでウィンドウに出す。
/// 本ログには「繋がった」だけを残す（ローダーのウィンドウの検査と本ログの文は、この語で終わりを見ている）。
///
/// `shape` は繋がったセッションの AGREE で決まった形（モードと、rollback なら D・delay 方式なら始めの遅れ。`LinkSlot::shape`）。
/// ウィンドウで選んだ形や ini の形ではなく決まった形を言う —— 両者が rollback を望んだときだけ rollback になるので、
/// 自分が rollback を選んでも相手次第で delay 方式になる。
pub(crate) fn connected_say(seat: Option<Seat>, shape: Option<(Mode, u8)>) -> Say {
    Say::new("繋がった", st::connected(ui_lang::current(), seat, shape)).kind(Kind::Connected)
}

/// 繋がったときに本ログへ 1 行残す、AGREE で決まった形。ウィンドウの文は本ログに残らないので、実機の回の合格条件が
/// 「決まった形が rollback か delay か」を本ログで見られるように別の行にする。「対戦の形: …で名乗る」の行（ini やウィンドウの選択）とは語が違う。
pub(crate) fn decided_shape_log(shape: Option<(Mode, u8)>) -> String {
    match shape {
        Some((Mode::Rollback, d)) => format!("対戦の形が決まった: rollback（D {d}）"),
        Some((Mode::Delay, d)) => format!("対戦の形が決まった: delay 方式（遅れ {d} フレーム）"),
        None => "対戦の形が決まった: 分からない".to_string(),
    }
}

fn internal(log: impl Into<String>) -> Say {
    Say::new(log, st::INTERNAL.get(ui_lang::current())).kind(Kind::Refused(Refusal::Internal))
}

/// 相手のコードを読めなかった理由を、試験のまとめの分類に（空は「まだ貼っていない」なので数えない）。
/// ウィンドウの文（`peer_error_text`）と同じ区分: タグの誤りは文字の誤りと同じ扱い。
fn refusal_of(e: PeerError) -> Option<Refusal> {
    Some(match e {
        PeerError::Empty => return None,
        PeerError::OwnCode => Refusal::OwnCode,
        PeerError::BothOffer => Refusal::BothOffer,
        PeerError::BothAnswer => Refusal::BothAnswer,
        PeerError::Code(CodeError::Expired { .. }) => Refusal::Expired,
        PeerError::Code(CodeError::WrongSession { .. }) => Refusal::WrongSession,
        PeerError::Code(CodeError::WrongSide(_)) => Refusal::WrongKind,
        PeerError::Code(CodeError::BadVersion(theirs)) => Refusal::OtherVersion { theirs },
        PeerError::Code(CodeError::OtherBuild { theirs }) => Refusal::OtherBuild { theirs },
        PeerError::Code(_) => Refusal::Broken,
    })
}

/// 取り違えと文字の誤りの文（人が次にすることを 1 つ添える）。戻りは（本ログの文, ウィンドウの文）。
fn peer_error_text(e: PeerError) -> (String, String) {
    let lang = ui_lang::current();
    let (log, show) = match e {
        PeerError::Empty => (
            "空（相手のコードを貼って保存する）".to_string(),
            st::PEER_EMPTY.get(lang),
        ),
        PeerError::OwnCode => (
            format!("自分の {MY_CODE_FILE} と同じ字（相手から受け取ったコードを貼る）"),
            st::OWN_CODE.get(lang),
        ),
        PeerError::BothOffer => (
            "2 人とも始める側（code_side = offer）。どちらか一方の ini を code_side = answer にして起こし直す"
                .to_string(),
            st::BOTH_OFFER.get(lang),
        ),
        PeerError::BothAnswer => (
            "2 人とも返す側（code_side = answer）。どちらか一方の ini を code_side = offer にして起こし直す"
                .to_string(),
            st::BOTH_ANSWER.get(lang),
        ),
        PeerError::Code(CodeError::Expired { .. }) => {
            return (
                "期限切れ（相手に新しいコードを頼む）".to_string(),
                st::expired(lang, code::DEFAULT_TTL_SECS),
            );
        }
        // 相手の HiganRTool のバージョンが違う（コードのバージョンはパケットのバージョンと同じ数）。本ログは日本語、ウィンドウは今の言語。
        PeerError::Code(CodeError::BadVersion(theirs)) => {
            let log = higanr_core::net_menu::other_version_text(higanr_core::wire::PROTO_VER, theirs);
            let show = higanr_core::net_menu::other_version_text_in(
                lang,
                higanr_core::wire::PROTO_VER,
                theirs,
            );
            return (log, show);
        }
        // 相手のビルドが違う。本ログは日本語、ウィンドウは今の言語（文字の原本は `net_menu` の表）。
        PeerError::Code(CodeError::OtherBuild { theirs }) => {
            let log =
                higanr_core::net_menu::other_build_text(higanr_core::update::BUILD_NUMBER, theirs);
            let show = higanr_core::net_menu::other_build_text_in(
                lang,
                higanr_core::update::BUILD_NUMBER,
                theirs,
            );
            return (log, show);
        }
        PeerError::Code(CodeError::WrongSession { .. }) => (
            format!("別の接続への返しのコード（今の {MY_CODE_FILE} を相手へ渡し直す）"),
            st::WRONG_SESSION.get(lang),
        ),
        PeerError::Code(e @ CodeError::WrongSide(_)) => (
            format!("相手のコードの種別かタグが合わない（{e:?}。相手に今のコードを頼む）"),
            st::WRONG_KIND.get(lang),
        ),
        // タグが合わないのは、途中で切れた・文字をコピーし間違えたときがいちばん多い（半分だけ貼ると検算で落ちる）ので、
        // ウィンドウでは文字の誤りと同じ文にする。
        PeerError::Code(e @ CodeError::BadTag) => (
            format!("相手のコードの種別かタグが合わない（{e:?}。相手に今のコードを頼む）"),
            st::BROKEN.get(lang),
        ),
        PeerError::Code(e) => (
            format!("字が壊れている（{e:?}。保存の途中なら、保存し終わったときに読み直す）"),
            st::BROKEN.get(lang),
        ),
    };
    (log, show.to_string())
}

/// 相手の候補のうちネットワークのガード（範囲 `scope`）を通るものだけ。1 つも無ければ断る。範囲の外の候補は黙って落とす
/// （相手は自分のアドレスを全部載せるので、LAN のアドレスと外のアドレスが混ざるのは普通。通るものが 1 つでもあれば繋ぎに行く）。
pub(crate) fn gate(cands: &Candidates, scope: Scope) -> Result<Candidates, Say> {
    let mut ok = Candidates::EMPTY;
    let mut refused = Vec::new();
    for c in cands.as_slice() {
        if guard::check_peer(c.addr, scope).is_ok() {
            // 読んだコードの候補は重ならず数の上限の中（`code` が確かめた）ので、足せないことは無い。
            let _ = ok.push(*c);
        } else {
            refused.push(c.addr);
        }
    }
    if ok.is_empty() {
        return Err(Say::new(
            format!(
                "相手の候補 {} にネットワークの範囲のガードを通るアドレスが無い（ネットワークの範囲 scope = {}。LAN の相手なら lan、外の相手なら internet を ini に書く）",
                refused
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" "),
                scope.name()
            ),
            // アドレスはウィンドウに出さない（本ログには残る）。
            st::no_route(ui_lang::current(), scope.name()),
        )
        .kind(Kind::Refused(Refusal::NoRoute)));
    }
    Ok(ok)
}

pub(crate) fn addrs_text(c: &Candidates) -> String {
    c.as_slice()
        .iter()
        .map(|x| x.addr.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 相手のファイルを `MAX_PEER_FILE` まで読む。無ければ None。
pub fn read_peer_file(dir: &Path) -> Result<Option<Vec<u8>>, String> {
    let p = dir.join(PEER_CODE_FILE);
    let f = match std::fs::File::open(&p) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{} を開けない: {e}", p.display())),
    };
    let mut b = Vec::new();
    f.take(MAX_PEER_FILE)
        .read_to_end(&mut b)
        .map_err(|e| format!("{} を読めない: {e}", p.display()))?;
    Ok(Some(b))
}

/// ウィンドウで選んだ対戦の形 `mode.txt`（ローダーが書く）を読む。無ければ None。上限は他のファイルと同じ。
pub fn read_mode_file(dir: &Path) -> Result<Option<Vec<u8>>, String> {
    let p = dir.join(MODE_FILE);
    let f = match std::fs::File::open(&p) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{} を開けない: {e}", p.display())),
    };
    let mut b = Vec::new();
    f.take(MAX_PEER_FILE)
        .read_to_end(&mut b)
        .map_err(|e| format!("{} を読めない: {e}", p.display()))?;
    Ok(Some(b))
}

/// 見張りが HELLO で名乗る形を決めるための材料（ウィンドウで選んだ形 `mode.txt` を採るか）。ゲームのスレッドには渡さない
/// （ファイルを読むのは見張りのスレッドだけ）。
#[derive(Debug, Clone)]
pub struct ModeCtx {
    /// None ならウィンドウの選択を見ない（検査の古い経路。`mode_wish` は `Config` の値のまま）。
    inner: Option<ModeInner>,
}

#[derive(Debug, Clone)]
struct ModeInner {
    settings: NetSettings,
    pid: u32,
    /// rollback のマッチの経路の入口を全部書けた実行か（`rollback_play_win::armed`）。
    wired: bool,
}

impl ModeCtx {
    /// ウィンドウの選択を見ない（検査の古い経路。本物は `new`）。
    #[cfg(test)]
    pub fn none() -> ModeCtx {
        ModeCtx { inner: None }
    }

    /// ウィンドウの選択を見る。`pid` は自分のプロセス番号、`wired` は `rollback_play_win::armed()` の値。
    pub fn new(settings: &NetSettings, pid: u32, wired: bool) -> ModeCtx {
        ModeCtx {
            inner: Some(ModeInner {
                settings: settings.clone(),
                pid,
                wired,
            }),
        }
    }

    /// `dir` の `mode.txt` を読み、名乗る形を決める。戻りは（決めた形、本ログに足す文）。ウィンドウの選択を見ない形では None。
    /// 読めない・別の実行のファイルは使わず（ini の形）、その旨を文に足す。
    fn choose(&self, dir: &Path) -> Option<(ModeChoice, Vec<String>)> {
        let m = self.inner.as_ref()?;
        let mut notes = Vec::new();
        let pick = match read_mode_file(dir) {
            Ok(None) => None,
            Ok(Some(b)) => match mode_pick::read_for(&b, m.pid) {
                FileRead::Mine(p) => Some(p),
                FileRead::OtherRun { pid } => {
                    notes.push(format!(
                        "{MODE_FILE} は別の実行（pid {pid}）のものなので使わない（この実行は pid {}）",
                        m.pid
                    ));
                    None
                }
                FileRead::Unreadable => {
                    notes.push(format!("{MODE_FILE} を読めない形なので使わない"));
                    None
                }
            },
            Err(e) => {
                notes.push(format!("{e}。使わない"));
                None
            }
        };
        let choice = m.settings.mode_choice(m.wired, pick);
        notes.push(choice.log_text());
        Some((choice, notes))
    }

    /// つなぐ前に決めた遅延（接続の始まりにホストが押下にする初めの値。`DelayPick::set_start`）: メニューの「対戦の遅延」で選んだ値
    /// `menu` があればそれ、無ければ ini の `delay`（auto か数。名乗る形 `named` の方式の値を見るが、両方とも同じ `delay` から作る）。
    /// 数も初めの値にするのは、メニューに出した数のまま始めるため（置かないと delay 方式は最初のマッチで推奨値へ替わる）。
    /// ウィンドウの選択を見ない形はメニューの値だけ。
    pub(crate) fn start_delay(
        &self,
        menu: Option<DelayChoice>,
        named: Mode,
    ) -> Option<DelayChoice> {
        let ini = self.inner.as_ref().map(|m| {
            let s = &m.settings;
            let (auto, n) = match named {
                Mode::Rollback => (s.rollback_delay_auto, s.rollback_delay),
                Mode::Delay => (s.latency_auto, s.latency),
            };
            if auto {
                DelayChoice::Auto
            } else {
                DelayChoice::Fixed(n)
            }
        });
        menu.or(ini)
    }

    /// ゲーム内のメニューで選んだ形 `pick`（変えていなければ `None` = ini の形）から名乗る形を決める（ファイルは読まない）。
    pub(crate) fn choose_pick(&self, pick: Option<Pick>) -> Option<(ModeChoice, Vec<String>)> {
        let m = self.inner.as_ref()?;
        let choice = m.settings.mode_choice(m.wired, pick);
        let notes = vec![choice.log_text()];
        Some((choice, notes))
    }
}

/// `status.txt` の中身（見出しの行と、ウィンドウの「状態」の行に出す 1 行）。ローダーは最後の行だけを出す（`code_relay::status_line`）。
pub(crate) fn status_text(side: CodeSide, secs: f32, show: &str) -> String {
    let side = match side {
        CodeSide::Offer => st::Side::Offer,
        CodeSide::Answer => st::Side::Answer,
        CodeSide::Spectate => st::Side::Spectate,
        CodeSide::Game => st::Side::Game,
    };
    let head = st::status_header(ui_lang::current(), side, secs);
    format!("{head}\r\n{show}\r\n")
}

/// ネットワークを開けなかった（見張りを起こす前に断った）ときのウィンドウの文。見張りが止められたときの文（`Watcher::run`）と同じ。
pub fn show_not_started() -> &'static str {
    st::NOT_STARTED.get(ui_lang::current())
}

/// 見張りを起こす前にネットワークを開けなかった（`bind = auto` でアドレスを決められない・受信ソケットを開けない等）ときに、`status.txt` へウィンドウの文を書く。
/// 書かないと、ローダーのウィンドウは `my_code.txt` も `status.txt` も来ないまま黙って待ち続け、利用者は何が起きたか分からない。
/// 書けなくてもネットワークの結果は変えない。
pub fn say_not_started(dir: &Path, side: CodeSide) {
    let _ = std::fs::create_dir_all(dir);
    let _ = replace_file(
        dir,
        STATUS_FILE,
        &status_text(side, 0.0, show_not_started()),
    );
}

/// 名前 `name` のファイルを書き直す（一時の名前に書いてから置き換える。人が書きかけのコードをコピーしないように）。
pub(crate) fn replace_file(dir: &Path, name: &str, text: &str) -> Result<(), String> {
    let tmp = dir.join(code_files::tmp_name(name));
    std::fs::write(&tmp, text).map_err(|e| format!("{} に書けない: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(name)).map_err(|e| format!("{name} へ置き換えられない: {e}"))
}

/// 自分のコードを `my_code.txt` に書く（末尾に改行。メモ帳で開いてコピーしやすいように）。
pub fn write_my_code(dir: &Path, text: &str) -> Result<(), String> {
    replace_file(dir, MY_CODE_FILE, &format!("{text}\r\n"))
}

/// `my_code.txt` を消す。中身が `own`（自分の書いたコード）でなければ消さない（人が別のものを置いた）。戻りは消したか。
pub fn remove_my_code(dir: &Path, own: Option<&str>) -> Result<bool, String> {
    let p = dir.join(MY_CODE_FILE);
    let b = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("{MY_CODE_FILE} を読めない: {e}")),
    };
    let text = String::from_utf8_lossy(&b);
    let bare = text.trim().trim_start_matches('\u{FEFF}').trim();
    if own.is_none_or(|o| o != bare) {
        return Err(format!(
            "{MY_CODE_FILE} の中身が自分の書いたコードと違うので消さない"
        ));
    }
    std::fs::remove_file(&p).map_err(|e| format!("{MY_CODE_FILE} を消せない: {e}"))?;
    Ok(true)
}

#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) notes: Mutex<Vec<String>>,
    pub(crate) stop: AtomicBool,
    facts: Mutex<CodeFacts>,
}

impl Shared {
    pub(crate) fn new(side: CodeSide, stun: Stun) -> Shared {
        Shared {
            notes: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            facts: Mutex::new(CodeFacts {
                stun,
                ..CodeFacts::new(side)
            }),
        }
    }

    pub(crate) fn note(&self, line: String) {
        let mut n = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        if n.len() >= NOTES_MAX {
            n.remove(0);
        }
        n.push(line);
    }

    pub(crate) fn count(&self, kind: Kind, secs: f32) {
        let mut f = self.facts.lock().unwrap_or_else(|e| e.into_inner());
        match kind {
            Kind::Info => {}
            Kind::PeerRead => {
                f.peer_read_at.get_or_insert(secs);
            }
            Kind::Refused(r) => {
                f.refused = f.refused.saturating_add(1);
                f.last_refusal = Some(r);
            }
            Kind::Connected => {
                f.connected_at.get_or_insert(secs);
            }
            Kind::EndedUnconnected => f.ended_unconnected = true,
            Kind::GaveUp => f.gave_up = true,
        }
    }
}

/// 見張りの持ち手。落とすと止めて待つ（本物の経路では落とさない —— `script_win` がプロセスの終わりまで持つ。見張りは
/// 繋がった・ネットワークが終わった・諦めたときに自分で終わる）。
pub struct Watch {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

impl Watch {
    pub(crate) fn from_parts(shared: Arc<Shared>, join: JoinHandle<()>) -> Watch {
        Watch {
            shared,
            join: Some(join),
        }
    }

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

    /// 止めて待つ（ゲーム内のメニューのつなぎ直しの片づけ。`net_code_game::close_parts`）。見張りの文と数は残る
    /// （試験のまとめが、次のつながりを開くまで最後のつながりの値を読む）。2 回目からは何もしない。
    pub fn stop(&mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.halt();
    }
}

/// 繋がったのを見た 1 回だけ見張りのスレッドが呼ぶ知らせ（本物は `notify_win::connected` —— ウィンドウの点滅と音）。
/// 戻りの文は本ログの「ネットワークの外」の行へ足す。検査は数えるだけの偽物を渡す。
pub type Notify = fn() -> String;

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub poll: Duration,
    /// セッションの設定を 1 度も置けないまま、これだけ経ったら諦める（ゲームのスレッドのネットワークは「1 通も届かない」で終わる）。
    pub window: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            poll: POLL,
            window: Duration::from_millis(u64::from(HANDSHAKE_RETRY_MS)),
        }
    }
}

/// 開く（読み取りのスレッド。`script_win::open_net` から。ネットワークを動かす前）: 置き場を作り、自分の候補（受信ソケットと、ini の `stun` があれば
/// 見えたアドレス）を決め、始める側なら秘密と session（ini に無ければ OS の乱数）で自分のコードを `my_code.txt` に書き、
/// 見張りのスレッドを起こす。戻りの文は本ログの「ネットワーク: 開いた」の行に足す（コードの文字は書かない。秘密を含むので、ファイルだけに置く）。
/// `mode` はウィンドウで選んだ対戦の形を採るための材料（`ModeCtx`）。
// 検査が個別に差し替える部品（見張りの間隔・繋がった知らせ・ウィンドウの選択）を引数で受ける入口で、本物は 1 回しか呼ばない。
#[allow(clippy::too_many_arguments)]
pub fn start(
    s: &NetSettings,
    dir: PathBuf,
    net: &mut GameNet,
    role_nonce: u64,
    slot: Arc<LinkSlot>,
    timing: Timing,
    notify: Notify,
    mode: ModeCtx,
) -> Result<(Watch, String), String> {
    let side = s
        .code_side
        .ok_or("code_side が無い（接続コードで受け渡す形でない）")?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("接続コードの置き場 {} を作れない: {e}", dir.display()))?;
    let mut note = String::new();
    let local = net.local_addr().ok_or("受信ソケットが分からない")?;
    // UPnP は STUN と並行に走らせる（結果は `nat_settle` で待つ）。
    net_udp::upnp_begin(s, net, &mut note);
    let (mapped, stun) = net_udp::stun_state(s, net, &mut note);
    let v6 = net_udp::open_ipv6(s, net, &mut note);
    let nat = net_udp::nat_settle(s, net, mapped, &mut note);
    // `candidates = mapped` で STUN の答えが無いときはここで断る（Err は `script_win::open_net` がウィンドウの文 `say_not_started` と
    // 本ログへ）。理由は STUN の側の文（`note`）を付けて返す。
    let own_cands = s
        .candidates_for_upnp(local, mapped, v6, nat.upnp_port)
        .map_err(|e| format!("{e}{note}"))?;
    net.note_own_candidates(own_cands.as_slice());
    let sw = match side {
        CodeSide::Offer => {
            let secret = match s.secret {
                Some(x) => x,
                None => net_udp::draw_secret()?,
            };
            let session = match s.session_id {
                Some(x) => x,
                None => draw_session()?,
            };
            let sw = Swapper::offer_with_nat(
                s,
                role_nonce,
                own_cands,
                nat.mark,
                secret,
                session,
                net_udp::now_unix(),
            )?;
            write_my_code(&dir, sw.own_text().unwrap_or_default())?;
            note.push_str(&format!(
                " / 接続コード（始める側 offer。session 0x{session:08X}、秘密は{}）を {} に書いた",
                if s.secret.is_some() {
                    "ini の値"
                } else {
                    "OS の乱数で引いた値"
                },
                dir.join(MY_CODE_FILE).display()
            ));
            sw
        }
        CodeSide::Answer => {
            // 前の回に書いた返すコードが残っていたら消す（人が古いものを渡さないように。自分の置き場の自分のファイル）。
            match std::fs::remove_file(dir.join(MY_CODE_FILE)) {
                Ok(()) => note.push_str(&format!(" / 前の回の {MY_CODE_FILE} を消した")),
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(format!("前の回の {MY_CODE_FILE} を消せない: {e}")),
            }
            note.push_str(&format!(
                " / 接続コード（返す側 answer）: {} に相手のコードが貼られるのを待つ",
                dir.join(PEER_CODE_FILE).display()
            ));
            Swapper::answer(s, role_nonce, own_cands).with_nat(nat.mark)
        }
        CodeSide::Spectate => {
            return Err(
                "見るだけの側（code_side = spectate）は接続コードの受け渡しの見張りを使わない"
                    .to_string(),
            );
        }
        CodeSide::Game => {
            return Err(
                "ゲーム内のメニューの側（code_side = game）は `start_game` の見張りを使う"
                    .to_string(),
            );
        }
    };
    let shared = Arc::new(Shared::new(side, stun));
    let t0 = Instant::now();
    let first = match side {
        CodeSide::Spectate | CodeSide::Game => Say::new("", ""),
        CodeSide::Offer => offer_waiting(code::DEFAULT_TTL_SECS),
        CodeSide::Answer => Say::new(
            format!(
                "相手の接続コード（始める側）が {PEER_CODE_FILE} に貼られて保存されるのを待っている"
            ),
            st::ANSWER_WAITING.get(ui_lang::current()),
        ),
    };
    let mut w = Watcher {
        sw,
        dir,
        slot,
        shared: Arc::clone(&shared),
        timing,
        notify,
        mode,
        t0,
        last_error: None,
    };
    w.say(&first);
    let join = std::thread::Builder::new()
        .name("higanr-code".into())
        .spawn(move || {
            // パニックは受けて外へ出さない（スレッドの入口）。受けたら見張りをやめる（ネットワークは相手の沈黙か時間切れで終わる）。
            let _ = catch_unwind(AssertUnwindSafe(|| w.run()));
        })
        .map_err(|e| format!("接続コードの見張りのスレッドを起こせない: {e}"))?;
    Ok((
        Watch {
            shared,
            join: Some(join),
        },
        note,
    ))
}

pub(crate) fn draw_session() -> Result<u32, String> {
    let mut b = [0u8; 4];
    if !os_random(&mut b) {
        return Err("session を OS の乱数で引けない".to_string());
    }
    Ok(u32::from_le_bytes(b))
}

struct Watcher {
    sw: Swapper,
    dir: PathBuf,
    slot: Arc<LinkSlot>,
    shared: Arc<Shared>,
    timing: Timing,
    notify: Notify,
    mode: ModeCtx,
    t0: Instant,
    last_error: Option<String>,
}

impl Watcher {
    fn say(&mut self, s: &Say) {
        let secs = self.t0.elapsed().as_secs_f32();
        self.shared.count(s.kind, secs);
        self.shared
            .note(format!("接続コード（{secs:.1} 秒）: {}", s.log));
        let _ = replace_file(
            &self.dir,
            STATUS_FILE,
            &status_text(self.sw.side(), secs, &s.show),
        );
    }

    fn run(&mut self) {
        loop {
            if self.shared.stop.load(Ordering::Relaxed) {
                // 持ち手が落とされた（本物の経路では、ネットワークのセッションを置けずに `open_net` が断ったときだけ）。コードは使われない
                // ので、秘密の載った `my_code.txt` を残さない。
                self.finish(Say::new(
                    "見張りを止めた（ネットワークを開けなかった）",
                    show_not_started(),
                ));
                return;
            }
            if self.slot.is_connected() {
                let shape = self.slot.shape();
                self.finish(connected_say(self.slot.seat(), shape));
                let secs = self.t0.elapsed().as_secs_f32();
                self.shared.note(format!(
                    "接続コード（{secs:.1} 秒）: {}",
                    decided_shape_log(shape)
                ));
                // ウィンドウの文を書き直した後で知らせる（音は鳴り終わるまで待つので、待つのはこのスレッドだけ）。秒は知らせ始めた時。
                let secs = self.t0.elapsed().as_secs_f32();
                let n = (self.notify)();
                self.shared.note(format!("接続コード（{secs:.1} 秒）: {n}"));
                return;
            }
            if self.slot.is_done() {
                self.finish(
                    Say::new(
                        "ネットワークが終わった（繋がらずに終えた）",
                        st::ENDED_UNCONNECTED.get(ui_lang::current()),
                    )
                    .kind(Kind::EndedUnconnected),
                );
                return;
            }
            if self.slot.generation() == 0 && self.t0.elapsed() >= self.timing.window {
                self.slot.give_up();
                let secs = self.timing.window.as_secs();
                self.finish(
                    Say::new(
                        format!(
                            "{secs} 秒の間に使えるコードを読めなかったので、ネットワークをやめた"
                        ),
                        st::gave_up(ui_lang::current(), secs),
                    )
                    .kind(Kind::GaveUp),
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
                    self.say(&peer_file_say(ui_lang::current(), &e));
                    self.last_error = Some(e);
                }
                return;
            }
        };
        match self.sw.on_peer_file(bytes.as_deref(), net_udp::now_unix()) {
            Step::Same => {}
            Step::Refused(why) => self.say(&why),
            Step::Ready {
                link,
                my_code,
                mut note,
            } => {
                if let Some(t) = my_code
                    && let Err(e) = write_my_code(&self.dir, &t)
                {
                    self.say(&reply_write_say(ui_lang::current(), &e));
                }
                // 名乗る形は、セッションの設定を置く直前にここで決める。ウィンドウで選んだ形のファイルを読むのはこのスレッドだけで、
                // ゲームのスレッドは置かれた値を取るだけ。
                let wish = match self.mode.choose(&self.dir) {
                    Some((choice, lines)) => {
                        let secs = self.t0.elapsed().as_secs_f32();
                        for l in lines {
                            self.shared.note(format!("接続コード（{secs:.1} 秒）: {l}"));
                        }
                        if let Some(extra) = choice.show_text(ui_lang::current()) {
                            note.show = format!("{} {extra}", note.show);
                        }
                        Some(choice.wish)
                    }
                    None => None,
                };
                self.slot.set_peer_nat(self.sw.peer_nat());
                self.slot.offer_with_mode(*link, wish);
                self.say(&note);
            }
        }
    }

    /// 終わり: `my_code.txt` を消して、文を残す。消せたこと・無かったことはウィンドウに出さない（ウィンドウは消えたことで終わりを知る）。
    /// 消せなかったときだけウィンドウにも出す（秘密を含むファイルが残る）。
    fn finish(&mut self, why: Say) {
        let removed = remove_my_code(&self.dir, self.sw.own_text());
        self.say(&finished_say(ui_lang::current(), why, removed));
    }
}

/// 相手のコードのファイルを読めなかった。`e`（日本語の誤りの文）は本ログに必ず残し、ウィンドウには言語に合う文だけを出す。
fn peer_file_say(lang: Lang, e: &str) -> Say {
    Say::new(e, st::peer_file_unreadable(lang, e)).kind(Kind::Refused(Refusal::Unreadable))
}

/// 返すコードをファイルに書けなかった。`e` の扱いは `peer_file_say` と同じ。
fn reply_write_say(lang: Lang, e: &str) -> Say {
    Say::new(e, st::reply_not_written(lang, e)).kind(Kind::Refused(Refusal::Internal))
}

/// 終わりの文（`why`）に、`my_code.txt` を消した結果を足す。消せなかった誤り `e` は本ログに必ず残し、ウィンドウには言語に合う文だけを足す。
fn finished_say(lang: Lang, why: Say, removed: Result<bool, String>) -> Say {
    let (removed, show) = match removed {
        Ok(true) => (format!("{MY_CODE_FILE} を消した"), why.show),
        Ok(false) => (format!("{MY_CODE_FILE} は無い"), why.show),
        Err(e) => {
            let show = st::my_code_not_deleted(lang, &why.show, &e);
            (e, show)
        }
    };
    Say::new(format!("{}。{removed}", why.log), show).kind(why.kind)
}

#[cfg(test)]
mod tests {
    //! ファイルは一時ディレクトリだけ（ゲームのフォルダは使わない）。接続コード・秘密・名前・アドレスは合成の値だけ。
    use super::*;
    use std::net::SocketAddr;
    use std::sync::atomic::AtomicU32;

    use higanr_core::handshake::{FailReason, ModeWish};
    use higanr_core::lang::{Lang, has_japanese};
    use higanr_core::wire::{ByeReason, Seat};

    use crate::netplay::fake::{FRAME_US, FakeGame, FakeNet};
    use crate::netplay::{self, CheckSource, End, NetPlay, Phase, WAIT_BUDGET_US};

    const SECRET: Secret = Secret([
        0x21, 0x43, 0x65, 0x87, 0xA9, 0xCB, 0xED, 0x0F, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE,
        0xF1,
    ]);
    const OTHER_SECRET: Secret = Secret([0x5A; 16]);
    const SID: u32 = 0x5EED_C0DE;
    const NOW: u64 = 1_900_000_000;

    fn lo(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn cands(addrs: &[&str]) -> Candidates {
        let v: Vec<SocketAddr> = addrs.iter().map(|a| a.parse().unwrap()).collect();
        Candidates::from_addrs(&v).unwrap()
    }

    fn settings(side: &str, name: &str) -> NetSettings {
        crate::net_config::from_text(&format!(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = {side}\nname = {name}\ndelay = 3\nhost_seat = 2\ncheck = digest\n"
        ))
        .unwrap()
        .unwrap()
    }

    fn offer_swapper(port: u16) -> Swapper {
        Swapper::offer(
            &settings("offer", "o"),
            0xF000,
            cands(&[&format!("127.0.0.1:{port}")]),
            SECRET,
            SID,
            NOW,
        )
        .unwrap()
    }

    fn answer_swapper(port: u16) -> Swapper {
        Swapper::answer(
            &settings("answer", "a"),
            0x0F00,
            cands(&[&format!("127.0.0.1:{port}")]),
        )
    }

    fn refused(s: Step) -> Say {
        match s {
            Step::Refused(w) => w,
            other => panic!("断っていない: {other:?}"),
        }
    }

    /// ウィンドウの文に本ログ向けの語が無いこと: ファイル名・ネットワーク・候補・秘密・session・アドレス・Rust の Debug の形。ウィンドウを読むのは接続コードを受け取った人。
    fn for_people(show: &str) {
        for w in [
            PEER_CODE_FILE,
            MY_CODE_FILE,
            "網",
            "候補",
            "秘密",
            "session",
            "127.0.0.1",
            "192.168.",
            "{",
            "(",
        ] {
            assert!(!show.contains(w), "窓の文に {w:?} が出た: {show}");
        }
    }

    /// ファイルの誤りの文（日本語）は、どの言語でも本ログに残り、英語のウィンドウには出ない。
    #[test]
    fn a_file_error_stays_in_the_log_and_the_english_window_has_no_japanese() {
        let io = "peer_code.txt を開けない: アクセスが拒否されました。 (os error 5)";
        for lang in Lang::ALL {
            let read = peer_file_say(lang, io);
            let write = reply_write_say(lang, io);
            let kept = finished_say(
                lang,
                Say::new("ネットワークが終わった", st::ENDED_UNCONNECTED.get(lang))
                    .kind(Kind::EndedUnconnected),
                Err(io.to_string()),
            );
            for say in [&read, &write, &kept] {
                assert!(say.log.contains(io), "本ログに誤りの文が無い: {say:?}");
                assert_eq!(has_japanese(&say.show), lang == Lang::Ja, "{say:?}");
                assert_eq!(say.show.contains("os error"), lang == Lang::Ja, "{say:?}");
            }
            assert_eq!(read.kind, Kind::Refused(Refusal::Unreadable));
            assert_eq!(write.kind, Kind::Refused(Refusal::Internal));
            assert_eq!(kept.kind, Kind::EndedUnconnected);
        }
        // 消せた・無かったときはウィンドウの文を変えず、本ログに結果だけを足す。
        let ok = finished_say(Lang::En, Say::new("l", "shown"), Ok(true));
        assert_eq!(
            (ok.log.as_str(), ok.show.as_str()),
            ("l。my_code.txt を消した", "shown")
        );
    }

    /// 取り違えと文字の誤りのウィンドウの文は、今の言語で出る。本ログの文は言語に関わらず日本語のまま。
    #[test]
    fn the_window_texts_follow_the_current_language_and_the_log_stays_japanese() {
        let errors = [
            PeerError::Empty,
            PeerError::OwnCode,
            PeerError::BothOffer,
            PeerError::BothAnswer,
            PeerError::Code(CodeError::Expired { expires: 1, now: 2 }),
            PeerError::Code(CodeError::BadVersion(8)),
            PeerError::Code(CodeError::OtherBuild { theirs: 5 }),
            PeerError::Code(CodeError::WrongSession { got: 1, want: 2 }),
            PeerError::Code(CodeError::WrongSide(3)),
            PeerError::Code(CodeError::BadTag),
            PeerError::Code(CodeError::BadLength),
        ];
        for e in errors {
            ui_lang::set(Lang::Ja);
            let (log_ja, show_ja) = peer_error_text(e);
            ui_lang::set(Lang::En);
            let (log_en, show_en) = peer_error_text(e);
            assert_eq!(log_ja, log_en, "{e:?}: 本ログの文が言語で変わった");
            assert!(has_japanese(&show_ja), "{e:?}: {show_ja}");
            assert!(
                !has_japanese(&show_en),
                "{e:?}: 英語の窓の文に日本語: {show_en}"
            );
            assert_ne!(show_ja, show_en, "{e:?}");
            for w in ["{", "PeerError", "CodeError", MY_CODE_FILE, PEER_CODE_FILE] {
                assert!(!show_en.contains(w), "{e:?}: 窓の文に {w:?}: {show_en}");
            }
        }
        ui_lang::set(Lang::En);
        assert_eq!(
            peer_error_text(PeerError::OwnCode).1,
            "You pasted your own connection code. Paste the code from your opponent."
        );
        let say = connected_say(Some(Seat::P1), Some((Mode::Rollback, 2)));
        assert_eq!(say.show, "Connected. You are 1P. (rollback, delay 2)");
        assert_eq!(
            say.log, "繋がった",
            "ローダーの窓の検査と本ログはこの語で終わりを見ている"
        );
        let head_and_line = status_text(CodeSide::Offer, 1.0, "x");
        assert_eq!(
            head_and_line,
            "HiganRTool connection code (offer side). 1.0 s after start:\r\nx\r\n"
        );
        assert!(!has_japanese(show_not_started()));
        ui_lang::set(Lang::Ja);
        assert!(has_japanese(show_not_started()));
        assert!(
            status_text(CodeSide::Offer, 1.0, "x")
                .starts_with("HiganRToolの接続コード：始める側（offer）")
        );
    }

    #[test]
    fn a_fresh_offer_swapper_waits_in_the_current_language() {
        ui_lang::set(Lang::En);
        let say = offer_waiting(code::DEFAULT_TTL_SECS);
        assert!(!has_japanese(&say.show), "{}", say.show);
        assert!(
            say.show.starts_with("Waiting for the reply code"),
            "{}",
            say.show
        );
        assert!(has_japanese(&say.log), "本ログは日本語のまま");
    }

    #[test]
    fn offer_and_answer_swap_codes_and_build_matching_links() {
        let mut off = offer_swapper(41_001);
        let mut ans = answer_swapper(41_002);
        assert_eq!(ans.own_text(), None, "返す側が相手のコードの前に書いた");
        let offer_text = off.own_text().unwrap().to_string();
        let pasted = format!("\u{FEFF}{offer_text}\r\n");
        let Step::Ready {
            link: la,
            my_code: Some(reply),
            note: na,
        } = ans.on_peer_file(Some(pasted.as_bytes()), NOW + 60)
        else {
            panic!("返す側が読めない")
        };
        assert_eq!(ans.own_text(), Some(reply.as_str()));
        let back = format!("{reply}\r\n");
        let Step::Ready {
            link: lf,
            my_code: None,
            note: nf,
        } = off.on_peer_file(Some(back.as_bytes()), NOW + 120)
        else {
            panic!("始める側が読めない")
        };
        assert!(
            na.show.contains("「o」") && na.show.contains("相手に送って"),
            "{na:?}"
        );
        assert!(
            nf.show.contains("「a」") && nf.show.contains("接続しています"),
            "{nf:?}"
        );
        assert!(na.log.contains("127.0.0.1:41001"), "{na:?}");
        // 相手の名前（"o" と "a"）は本ログ向けの文に書かない。
        assert!(
            !na.log.contains("\"o\"") && !nf.log.contains("\"a\""),
            "{na:?} {nf:?}"
        );
        for_people(&na.show);
        for_people(&nf.show);
        for l in [&la, &lf] {
            assert_eq!((l.handshake.session_id, l.secret), (SID, SECRET));
            assert_eq!((l.handshake.latency, l.handshake.host_seat), (3, Seat::P2));
        }
        assert_eq!(la.peers, cands(&["127.0.0.1:41001"]));
        assert_eq!(lf.peers, cands(&["127.0.0.1:41002"]));
        assert_eq!(
            (lf.handshake.role_nonce, la.handshake.role_nonce),
            (0xF000, 0x0F00)
        );
        assert_eq!(
            (lf.handshake.name, la.handshake.name),
            (Name::new("o"), Name::new("a"))
        );
        assert_eq!(
            off.on_peer_file(Some(back.as_bytes()), NOW + 121),
            Step::Same
        );
        assert_eq!(off.on_peer_file(None, NOW + 122), Step::Same);
        assert_eq!(
            ans.on_peer_file(Some(pasted.as_bytes()), NOW + 123),
            Step::Same
        );
    }

    #[test]
    fn a_changed_file_is_read_again_and_an_unchanged_one_is_not() {
        let mut off = offer_swapper(41_001);
        let mut ans = answer_swapper(41_002);
        let offer_text = off.own_text().unwrap().to_string();
        let Step::Ready {
            my_code: Some(reply),
            ..
        } = ans.on_peer_file(Some(offer_text.as_bytes()), NOW)
        else {
            panic!()
        };
        let half = &reply[..reply.len() / 2];
        let why = refused(off.on_peer_file(Some(half.as_bytes()), NOW));
        assert!(why.log.contains("を読めない"), "{why:?}");
        assert!(why.show.contains("欠けている"), "{why:?}");
        for_people(&why.show);
        assert_eq!(off.on_peer_file(Some(half.as_bytes()), NOW), Step::Same);
        assert!(matches!(
            off.on_peer_file(Some(reply.as_bytes()), NOW),
            Step::Ready { .. }
        ));
        let mut off2 = Swapper::offer(
            &settings("offer", "o"),
            0xF000,
            cands(&["127.0.0.1:41003"]),
            OTHER_SECRET,
            SID ^ 0xFFFF,
            NOW,
        )
        .unwrap();
        let Step::Ready {
            link: l2,
            my_code: Some(reply2),
            ..
        } = ans.on_peer_file(Some(off2.own_text().unwrap().as_bytes()), NOW + 1)
        else {
            panic!("別の始める側のコードを読み直さない")
        };
        assert_ne!(reply2, reply);
        assert_eq!(
            (l2.handshake.session_id, l2.secret, l2.peers),
            (SID ^ 0xFFFF, OTHER_SECRET, cands(&["127.0.0.1:41003"]))
        );
        assert!(matches!(
            off2.on_peer_file(Some(reply2.as_bytes()), NOW + 2),
            Step::Ready { .. }
        ));
    }

    #[test]
    fn mixups_are_named_so_a_person_can_fix_them() {
        let mut off = offer_swapper(41_001);
        let other_offer = Swapper::offer(
            &settings("offer", "x"),
            1,
            cands(&["127.0.0.1:41009"]),
            OTHER_SECRET,
            SID ^ 1,
            NOW,
        )
        .unwrap();
        let mut shows = Vec::new();
        let mut kinds = Vec::new();
        let why = refused(off.on_peer_file(Some(other_offer.own_text().unwrap().as_bytes()), NOW));
        assert!(why.log.contains("2 人とも始める側"), "{why:?}");
        assert!(why.log.contains("code_side = answer"), "{why:?}");
        assert!(why.show.contains("code_sideをanswer"), "{why:?}");
        kinds.push(why.kind);
        shows.push(why.show);
        let own = off.own_text().unwrap().to_string();
        let why = refused(off.on_peer_file(Some(format!("{own}\r\n").as_bytes()), NOW));
        assert!(why.log.contains("自分の my_code.txt と同じ字"), "{why:?}");
        assert!(why.show.contains("自分の接続コード"), "{why:?}");
        kinds.push(why.kind);
        shows.push(why.show);
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(own.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        let why = refused(off.on_peer_file(Some(&utf16), NOW));
        assert!(why.log.contains("UTF-16"), "{why:?}");
        kinds.push(why.kind);
        shows.push(why.show);
        let why = refused(off.on_peer_file(Some(b"\r\n"), NOW));
        assert!(why.log.contains("空"), "{why:?}");
        assert_eq!(why.kind, Kind::Info, "空のファイルを読めなかった数に入れた");
        shows.push(why.show);

        let mut ans = answer_swapper(41_002);
        let Step::Ready {
            my_code: Some(reply),
            ..
        } = ans.on_peer_file(Some(own.as_bytes()), NOW)
        else {
            panic!()
        };
        let mut ans2 = answer_swapper(41_004);
        let why = refused(ans2.on_peer_file(Some(reply.as_bytes()), NOW));
        assert!(why.log.contains("2 人とも返す側"), "{why:?}");
        assert!(why.log.contains("code_side = offer"), "{why:?}");
        assert!(why.show.contains("code_sideをoffer"), "{why:?}");
        kinds.push(why.kind);
        shows.push(why.show);
        let why =
            refused(ans2.on_peer_file(Some(own.as_bytes()), NOW + code::DEFAULT_TTL_SECS + 1));
        assert!(why.log.contains("期限切れ"), "{why:?}");
        assert!(why.show.contains("期限"), "{why:?}");
        kinds.push(why.kind);
        assert_eq!(
            kinds,
            [
                Kind::Refused(Refusal::BothOffer),
                Kind::Refused(Refusal::OwnCode),
                Kind::Refused(Refusal::Unreadable),
                Kind::Refused(Refusal::BothAnswer),
                Kind::Refused(Refusal::Expired),
            ]
        );
        assert!(
            why.show
                .contains(&format!("（{}）", code::span_text(code::DEFAULT_TTL_SECS))),
            "{why:?}"
        );
        shows.push(why.show);
        assert_eq!(ans2.own_text(), None, "読めないのに返すコードを作った");
        for s in &shows {
            for_people(s);
        }
        let mut uniq = shows.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), shows.len(), "{shows:?}");
    }

    #[test]
    fn the_window_texts_are_built_from_the_code_lifetime() {
        let want = code::span_text(code::DEFAULT_TTL_SECS);
        for (secs, span) in [(45 * 60, "45分"), (90, "90秒"), (7_200, "120分")] {
            let e = st::expired(higanr_core::lang::Lang::Ja, secs);
            let w = offer_waiting(secs);
            assert!(e.contains(&format!("（{span}）")), "{e}");
            assert!(w.show.contains(&format!("は{span}で使えなく")), "{w:?}");
            assert!(w.log.contains(&format!("有効期限 {span}")), "{w:?}");
            for_people(&e);
            for_people(&w.show);
        }
        let d = temp_dir("ttl_text");
        let s = settings("offer", "a");
        let (mut n, _r) = net_udp::open_for_code(s.bind.fixed().unwrap()).unwrap();
        let (w, _) = start(
            &s,
            d.clone(),
            &mut n,
            1,
            Arc::new(LinkSlot::new()),
            FAST,
            quiet,
        )
        .unwrap();
        let status = std::fs::read_to_string(d.join(STATUS_FILE)).unwrap();
        assert!(
            status.contains(&format!("は{want}で使えなく")),
            "本物の期限の長さ {want} が窓の文に無い: {status}"
        );
        drop(w);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_connected_text_says_which_seat_you_are() {
        for (seat, want) in [
            (Some(Seat::P1), "接続しました。あなたは1Pです。"),
            (Some(Seat::P2), "接続しました。あなたは2Pです。"),
            (None, "接続しました。"),
        ] {
            let say = connected_say(seat, None);
            assert_eq!(say.show, want);
            assert_eq!(say.log, "繋がった");
            for_people(&say.show);
            // 本ログの 1 行（実機の合格条件が読む文字列。変えたら scripts/runs/m8_4_*.run も直す）。
            assert_eq!(
                decided_shape_log(Some((Mode::Rollback, 0))),
                "対戦の形が決まった: rollback（D 0）"
            );
            assert_eq!(
                decided_shape_log(Some((Mode::Delay, 2))),
                "対戦の形が決まった: delay 方式（遅れ 2 フレーム）"
            );
            for (shape, tail) in [
                ((Mode::Rollback, 1), "（rollback・ディレイ 1 フレーム）"),
                ((Mode::Rollback, 0), "（rollback・ディレイ 0 フレーム）"),
                ((Mode::Delay, 2), "（delay方式・ディレイ 2 フレーム）"),
            ] {
                let say = connected_say(seat, Some(shape));
                assert_eq!(say.show, format!("{want}{tail}"));
                assert_eq!(say.log, "繋がった");
                for_people(&say.show);
            }
        }
        for (seat, mode, d) in [
            (Seat::P1, Mode::Rollback, 2),
            (Seat::P2, Mode::Delay, 3),
            (Seat::P1, Mode::Delay, 0),
        ] {
            let s = LinkSlot::new();
            assert_eq!((s.is_connected(), s.seat(), s.shape()), (false, None, None));
            s.mark_connected(seat, mode, d);
            assert_eq!(
                (s.is_connected(), s.seat(), s.shape()),
                (true, Some(seat), Some((mode, d)))
            );
        }
        let slot = Arc::new(LinkSlot::new());
        assert_eq!(slot.seat(), None);
        let mut d = Duo::new(&slot);
        slot.offer(lc(2, 0xF000, SECRET));
        d.run(120);
        let seat_a = d.a.connected().expect("繋がらない").seat();
        assert!(slot.is_connected());
        assert_eq!(
            slot.seat(),
            Some(seat_a),
            "セッションの席が見張りへ渡らない"
        );
        let seat_b = d.b.connected().expect("B が繋がらない").seat();
        assert_ne!(seat_a, seat_b, "2 人が同じ席");
        assert_eq!(slot.seat(), Some(Seat::P2));
        let est = d.a.connected().unwrap().established();
        assert_eq!(slot.shape(), Some((est.mode, est.delay)));
        assert_eq!(slot.shape(), Some((Mode::Delay, 2)));
    }

    /// ウィンドウで選んだ形（`LinkSlot::offer_with_mode` の上書き）が、ゲームのスレッドのセッションの望むモードになる検査用の 2 人。
    /// A は接続コードの形、B は相手の候補を知っている今までの形。偽のゲームは選択の画面に居続け、決まったモード・D だけを見る。
    fn duo_with_wishes(slot: &Arc<LinkSlot>, cfg_a: ModeWish, wish_b: ModeWish) -> Duo {
        let (mut na, mut nb, clock) = FakeNet::pair(lo(1), lo(2), 2_000);
        let cfg = |w: ModeWish| netplay::Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            mode_wish: w,
            ..netplay::Config::default()
        };
        let mut ga = FakeGame::new(0xA, 30, 1, 1);
        let mut gb = FakeGame::new(0xB, 30, 1, 1);
        ga.menu_before_decide = u32::MAX;
        gb.menu_before_decide = u32::MAX;
        Duo {
            a: NetPlay::with_code_slot(Arc::clone(slot), cfg(cfg_a), &mut na),
            ga,
            na,
            b: NetPlay::new(lc(1, 1, SECRET), cfg(wish_b), &mut nb),
            gb,
            nb,
            clock,
            alloc_a: 0,
        }
    }

    #[test]
    fn the_game_side_names_the_window_pick_instead_of_the_ini_shape() {
        let (rb, dl) = (ModeWish::rollback, ModeWish::DELAY);
        // (設定 A, 上書き A, 望む B, 決まる形)。A は nonce が大きいのでホスト（D は A の値）。
        type Case = (ModeWish, Option<ModeWish>, ModeWish, (Mode, u8));
        let cases: [Case; 6] = [
            (rb(1), None, rb(1), (Mode::Rollback, 1)),
            (dl, None, rb(1), (Mode::Delay, 2)),
            (dl, Some(rb(0)), rb(1), (Mode::Rollback, 0)),
            (rb(1), Some(rb(2)), rb(1), (Mode::Rollback, 2)),
            (rb(1), Some(dl), rb(1), (Mode::Delay, 2)),
            (rb(1), Some(rb(0)), dl, (Mode::Delay, 2)),
        ];
        for (i, (cfg_a, over, wish_b, want)) in cases.into_iter().enumerate() {
            let slot = Arc::new(LinkSlot::new());
            let mut d = duo_with_wishes(&slot, cfg_a, wish_b);
            slot.offer_with_mode(lc(2, 0xF000, SECRET), over);
            d.run(120);
            assert!(d.a.connected().is_some(), "{i}: 繋がらない {:?}", d.a.end());
            assert!(d.b.connected().is_some(), "{i}: B が繋がらない");
            assert_eq!(slot.shape(), Some(want), "ケース {i}");
            let est = d.b.connected().unwrap().established();
            assert_eq!((est.mode, est.delay), want, "ケース {i}: B の見る形");
        }
    }

    fn mode_settings(extra: &str) -> NetSettings {
        crate::net_config::from_text(&format!(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = answer\nname = m\n{extra}"
        ))
        .unwrap()
        .unwrap()
    }

    #[test]
    fn the_menu_and_the_start_show_the_ini_delay_in_both_modes() {
        use higanr_core::delay_pick::DelayChoice::{self, Auto, Fixed};
        let cases: [(&str, DelayChoice); 6] = [
            ("", Auto),
            ("delay = auto\n", Auto),
            ("delay = 3\n", Fixed(3)),
            ("delay = 0\n", Fixed(0)),
            ("latency = 6\nrollback_delay = 5\n", Auto),
            ("latency = 6\nrollback_delay = 5\ndelay = 4\n", Fixed(4)),
        ];
        for mode in [Mode::Rollback, Mode::Delay] {
            let word = if mode == Mode::Rollback {
                "rollback"
            } else {
                "delay"
            };
            for (extra, want) in cases {
                let s = mode_settings(&format!("mode = {word}\n{extra}"));
                let (shape, auto) = crate::script_win::menu_start(&s);
                let sel = higanr_core::net_menu::Menu::new(shape)
                    .with_start_auto(auto)
                    .shape();
                assert_eq!((sel.mode, sel.delay), (mode, want), "{word}: {extra:?}");
                assert_eq!(
                    ModeCtx::new(&s, 1, true).start_delay(None, mode),
                    Some(want),
                    "{word}: {extra:?}"
                );
            }
        }
    }

    #[test]
    fn the_start_delay_is_the_menu_choice_then_the_ini_delay_in_both_modes() {
        use higanr_core::delay_pick::DelayChoice::{Auto, Fixed};
        let auto = mode_settings("delay = auto\n");
        let unwritten = mode_settings("");
        let three = mode_settings("delay = 3\n");
        let zero = mode_settings("mode = delay\ndelay = 0\n");
        // 古いキーは読まない（どちらの方式でも `delay` を書いていない形と同じ auto）。
        let old = mode_settings("latency = 5\nrollback_delay = 4\n");
        let ctx = |s: &NetSettings| ModeCtx::new(s, 1, true);
        assert_eq!(
            ctx(&auto).start_delay(Some(Fixed(3)), Mode::Rollback),
            Some(Fixed(3))
        );
        assert_eq!(ctx(&three).start_delay(Some(Auto), Mode::Delay), Some(Auto));
        for named in [Mode::Rollback, Mode::Delay] {
            assert_eq!(ctx(&auto).start_delay(None, named), Some(Auto), "{named:?}");
            assert_eq!(
                ctx(&unwritten).start_delay(None, named),
                Some(Auto),
                "{named:?}"
            );
            assert_eq!(ctx(&old).start_delay(None, named), Some(Auto), "{named:?}");
            assert_eq!(
                ctx(&three).start_delay(None, named),
                Some(Fixed(3)),
                "{named:?}"
            );
            assert_eq!(
                ctx(&zero).start_delay(None, named),
                Some(Fixed(0)),
                "{named:?}"
            );
            assert_eq!(ModeCtx::none().start_delay(None, named), None);
        }
        assert_eq!(
            ModeCtx::none().start_delay(Some(Fixed(1)), Mode::Rollback),
            Some(Fixed(1))
        );
    }

    #[test]
    fn the_mode_file_is_taken_only_for_my_pid_and_only_when_it_can_be_named() {
        let d = temp_dir("mode_file");
        const PID: u32 = 4242;
        let rb = settings_rb();
        let ctx = |s: &NetSettings, wired: bool| ModeCtx::new(s, PID, wired);
        let put = |text: &str| std::fs::write(d.join(MODE_FILE), text).unwrap();
        let mine = |pick: &str| mode_pick::render(PID, mode_pick::Pick::parse(pick).unwrap());
        let wish = |c: &ModeCtx| {
            let (ch, notes) = c.choose(&d).expect("窓の選択を見る形のはず");
            ((ch.wish.mode, ch.wish.rollback_delay), notes.join(" | "))
        };

        let ((m, dd), notes) = wish(&ctx(&rb, true));
        assert_eq!((m, dd), (Mode::Rollback, 4), "{notes}");
        assert!(
            notes.contains("ini の形で名乗る: rollback（D 4）"),
            "{notes}"
        );
        assert!(
            !notes.contains(MODE_FILE),
            "ファイルが無いのに言及した: {notes}"
        );

        for (pick, want) in [
            ("0", (Mode::Rollback, 0)),
            ("2", (Mode::Rollback, 2)),
            ("delay", (Mode::Delay, 4)),
        ] {
            put(&mine(pick));
            let (got, notes) = wish(&ctx(&rb, true));
            assert_eq!(got, want, "{pick}: {notes}");
            assert!(notes.contains("窓で選んだ形で名乗る"), "{pick}: {notes}");
        }

        // pid の違う（前の実行の）ファイルは使わない。理由が本ログに残る。
        put(&mode_pick::render(PID + 1, mode_pick::Pick::Delay));
        let (got, notes) = wish(&ctx(&rb, true));
        assert_eq!(got, (Mode::Rollback, 4), "{notes}");
        assert!(
            notes.contains(&format!("別の実行（pid {}）", PID + 1)),
            "{notes}"
        );
        assert!(notes.contains("ini の形で名乗る"), "{notes}");

        for bad in [
            &b"pid = 4242\r\npick = 9\r\n"[..],
            &[0xFF, 0xFE, 0x00][..],
            b"",
        ] {
            std::fs::write(d.join(MODE_FILE), bad).unwrap();
            let (got, notes) = wish(&ctx(&rb, true));
            assert_eq!(got, (Mode::Rollback, 4), "{bad:?}: {notes}");
            assert!(notes.contains("読めない形"), "{bad:?}: {notes}");
        }

        // ウィンドウで rollback を選んでも、入口を書けていない実行・ini が delay の実行は rollback を名乗らない。
        put(&mine("1"));
        let (got, notes) = wish(&ctx(&rb, false));
        assert_eq!(
            got.0,
            Mode::Delay,
            "入口が無いのに rollback を名乗った: {notes}"
        );
        assert!(notes.contains("使えない"), "{notes}");
        let dl = mode_settings("mode = delay\ndelay = 3\n");
        for wired in [false, true] {
            let (got, notes) = wish(&ctx(&dl, wired));
            assert_eq!(got, (Mode::Delay, 3), "wired={wired}: {notes}");
            assert!(notes.contains("mode = delay"), "{notes}");
        }
        let (ch, _) = ctx(&dl, false).choose(&d).unwrap();
        assert!(
            ch.show_text(higanr_core::lang::Lang::Ja).is_some(),
            "窓に理由を出さない"
        );
        std::fs::remove_file(d.join(MODE_FILE)).unwrap();
        let (ch, _) = ctx(&dl, false).choose(&d).unwrap();
        assert_eq!(
            (ch.wish.mode, ch.show_text(higanr_core::lang::Lang::Ja)),
            (Mode::Delay, None)
        );

        assert!(ModeCtx::none().choose(&d).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn settings_rb() -> NetSettings {
        mode_settings("mode = rollback\ndelay = 4\n")
    }

    #[test]
    fn the_watcher_reads_the_mode_file_before_it_offers_the_link() {
        let offer_text = {
            let off = Swapper::offer(
                &settings("offer", "o"),
                1,
                cands(&["127.0.0.1:41001"]),
                SECRET,
                SID,
                net_udp::now_unix(),
            )
            .unwrap();
            off.own_text().unwrap().to_string()
        };
        use mode_pick::Pick;
        let me = std::process::id();
        // (名前, mode.txt（pid と選択。無ければ None）, wired, 期待する名乗り（モードと D）, ウィンドウの文に出る語)。ini は既定（rollback・D 1）。
        type Case = (
            &'static str,
            Option<(u32, Pick)>,
            bool,
            (Mode, u8),
            Option<&'static str>,
        );
        let cases: [Case; 5] = [
            ("none", None, true, (Mode::Rollback, 1), None),
            (
                "mine_delay",
                Some((me, Pick::Delay)),
                true,
                (Mode::Delay, 1),
                None,
            ),
            (
                "mine_d2",
                Some((me, Pick::Rollback(2))),
                true,
                (Mode::Rollback, 2),
                None,
            ),
            (
                "other_pid",
                Some((me + 1, Pick::Delay)),
                true,
                (Mode::Rollback, 1),
                None,
            ),
            (
                "unwired",
                Some((me, Pick::Rollback(2))),
                false,
                (Mode::Delay, 1),
                Some("rollbackの準備ができなかった"),
            ),
        ];
        for (tag, file, wired, want, say) in cases {
            let d = temp_dir(&format!("watch_mode_{tag}"));
            if let Some((p, pick)) = file {
                std::fs::write(d.join(MODE_FILE), mode_pick::render(p, pick)).unwrap();
            }
            let s = mode_settings("");
            let (mut n, _r) = net_udp::open_for_code(s.bind.fixed().unwrap()).unwrap();
            let slot = Arc::new(LinkSlot::new());
            let (w, _) = super::start(
                &s,
                d.clone(),
                &mut n,
                2,
                Arc::clone(&slot),
                FAST,
                quiet,
                ModeCtx::new(&s, me, wired),
            )
            .unwrap();
            std::fs::write(d.join(PEER_CODE_FILE), &offer_text).unwrap();
            wait_until("セッションの設定が置かれる", 10, || {
                slot.generation() > 0
            });
            // 見張りはセッションに置いてから「状態」の行を書く。置かれたのを見た直後に読むと書く前の行を読みうるので、読んだ旨の行が出るまで待つ。
            wait_until("状態の行が書かれる", 10, || {
                std::fs::read_to_string(d.join(STATUS_FILE))
                    .is_ok_and(|s| s.contains("コードを読みました"))
            });
            let got = slot.offered_wish().expect("設定が無い");
            let got = got.expect("見張りが上書きを置かなかった（mode.txt を読んでいない）");
            assert_eq!((got.mode, got.rollback_delay), want, "{tag}");
            let notes = w.notes().join("\n");
            assert!(
                notes.contains("対戦の形: "),
                "{tag}: 本ログの 1 行が無い\n{notes}"
            );
            let status = std::fs::read_to_string(d.join(STATUS_FILE)).unwrap();
            match say {
                Some(t) => assert!(status.contains(t), "{tag}: {status}"),
                None => assert!(!status.contains("rollbackは使えません"), "{tag}: {status}"),
            }
            for_people(&status);
            drop(w);
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn the_file_name_rules_live_only_in_core() {
        let dll = include_str!("net_code.rs");
        let loader = include_str!("../../loader/src/code_relay.rs");
        for (who, src) in [("net_code.rs", dll), ("code_relay.rs", loader)] {
            let body = &src[..src.find("#[cfg(test)]").expect("検査の節が無い")];
            for lit in [
                "\"my_code.txt\"",
                "\"peer_code.txt\"",
                "\"status.txt\"",
                "\"mode.txt\"",
                "\"code\"",
                ".tmp\"",
            ] {
                assert!(
                    !body.contains(lit),
                    "{who} の本体に {lit} がある（core の code_files へ寄せる）"
                );
            }
        }
    }

    #[test]
    fn peer_candidates_pass_the_gate_before_the_link() {
        for (their, want) in [
            (vec!["192.168.0.2:41001"], None),
            (vec!["192.0.2.7:41001", "[2001:db8::7]:41001"], None),
            (
                vec!["192.168.0.2:41001", "127.0.0.1:41001"],
                Some(cands(&["127.0.0.1:41001"])),
            ),
        ] {
            let off = Swapper::offer(&settings("offer", "o"), 1, cands(&their), SECRET, SID, NOW)
                .unwrap();
            let mut ans = answer_swapper(41_002);
            let got = ans.on_peer_file(Some(off.own_text().unwrap().as_bytes()), NOW);
            match (got, want) {
                (Step::Ready { link, .. }, Some(w)) => assert_eq!(link.peers, w, "{their:?}"),
                (Step::Refused(why), None) => {
                    assert_eq!(why.kind, Kind::Refused(Refusal::NoRoute), "{their:?}");
                    assert!(
                        why.log.contains("ネットワークの範囲のガード"),
                        "{their:?}: {why:?}"
                    );
                    assert!(why.log.contains(":41001"), "{their:?}: {why:?}");
                    assert!(!why.show.contains(":41001"), "{their:?}: {why:?}");
                    for_people(&why.show);
                }
                (g, w) => panic!("{their:?}: {g:?} / 期待 {w:?}"),
            }
        }
    }

    #[test]
    fn peer_candidates_follow_the_scope_in_the_ini() {
        let their = [
            "203.0.113.7:41001",
            "192.168.0.2:41001",
            "[2001:db8::7]:41001",
            "172.16.5.5:41001",
        ];
        for (scope, want) in [
            ("loopback", None),
            (
                "lan",
                Some(cands(&["192.168.0.2:41001", "172.16.5.5:41001"])),
            ),
            ("internet", Some(cands(&their))),
        ] {
            let off = Swapper::offer(&settings("offer", "o"), 1, cands(&their), SECRET, SID, NOW)
                .unwrap();
            let s = crate::net_config::from_text(&format!(
                "[netplay]\nenable = 1\nscope = {scope}\nbind = 127.0.0.1:0\ncode_side = answer\nname = a\n"
            ))
            .unwrap()
            .unwrap();
            let mut ans = Swapper::answer(&s, 0x0F00, cands(&["127.0.0.1:41002"]));
            let got = ans.on_peer_file(Some(off.own_text().unwrap().as_bytes()), NOW);
            match (got, want) {
                (Step::Ready { link, .. }, Some(w)) => assert_eq!(link.peers, w, "{scope}"),
                (Step::Refused(why), None) => {
                    assert!(why.log.contains("scope = loopback"), "{scope}: {why:?}");
                    assert!(why.show.contains("scope = loopback"), "{scope}: {why:?}");
                    assert!(why.show.contains("lan") && why.show.contains("internet"));
                }
                (g, w) => panic!("{scope}: {g:?} / 期待 {w:?}"),
            }
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("higanrtool_net_code_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn code_dir_is_under_the_dll_folder_unless_absolute() {
        let dll = Path::new(r"C:\tool\bin");
        assert_eq!(
            code_dir(Some(dll), None).unwrap(),
            PathBuf::from(r"C:\tool\bin\code")
        );
        assert_eq!(
            code_dir(Some(dll), Some("code_b")).unwrap(),
            PathBuf::from(r"C:\tool\bin\code_b")
        );
        assert_eq!(
            code_dir(Some(dll), Some(r"D:\x\codes")).unwrap(),
            PathBuf::from(r"D:\x\codes")
        );
        assert!(code_dir(None, None).is_err());
        assert!(code_dir(None, Some(r"D:\x\codes")).is_ok());
    }

    #[test]
    fn my_code_is_written_and_removed_only_if_it_is_ours() {
        let d = temp_dir("files");
        write_my_code(&d, "AAAA").unwrap();
        assert_eq!(std::fs::read(d.join(MY_CODE_FILE)).unwrap(), b"AAAA\r\n");
        assert!(!d.join(format!("{MY_CODE_FILE}.tmp")).exists());
        write_my_code(&d, "BBBB").unwrap();
        assert_eq!(std::fs::read(d.join(MY_CODE_FILE)).unwrap(), b"BBBB\r\n");
        assert!(remove_my_code(&d, Some("AAAA")).is_err());
        assert!(d.join(MY_CODE_FILE).exists(), "自分のでないものを消した");
        assert!(remove_my_code(&d, None).is_err());
        assert_eq!(remove_my_code(&d, Some("BBBB")), Ok(true));
        assert!(!d.join(MY_CODE_FILE).exists());
        assert_eq!(remove_my_code(&d, Some("BBBB")), Ok(false));
        std::fs::write(d.join(MY_CODE_FILE), "\u{FEFF}CCCC\r\n").unwrap();
        assert_eq!(remove_my_code(&d, Some("CCCC")), Ok(true));

        assert_eq!(read_peer_file(&d), Ok(None));
        std::fs::write(d.join(PEER_CODE_FILE), vec![b'A'; 10_000]).unwrap();
        assert_eq!(
            read_peer_file(&d).unwrap().map(|b| b.len() as u64),
            Some(MAX_PEER_FILE)
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    struct Duo {
        a: NetPlay,
        ga: FakeGame,
        na: FakeNet,
        b: NetPlay,
        gb: FakeGame,
        nb: FakeNet,
        clock: std::rc::Rc<std::cell::Cell<u64>>,
        alloc_a: u64,
    }

    impl Duo {
        fn new(slot: &Arc<LinkSlot>) -> Duo {
            let (mut na, mut nb, clock) = FakeNet::pair(lo(1), lo(2), 2_000);
            let cfg = netplay::Config {
                check: CheckSource::InputDigest,
                wait_budget_us: WAIT_BUDGET_US,
                ..netplay::Config::default()
            };
            let a = NetPlay::with_code_slot(Arc::clone(slot), cfg, &mut na);
            let b = NetPlay::new(lc(1, 1, SECRET), cfg, &mut nb);
            Duo {
                a,
                ga: FakeGame::new(0xA, 30, 1, 1),
                na,
                b,
                gb: FakeGame::new(0xB, 30, 1, 1),
                nb,
                clock,
                alloc_a: 0,
            }
        }

        fn run(&mut self, n: u32) {
            for _ in 0..n {
                self.clock.set(self.clock.get() + FRAME_US);
                self.ga.begin();
                let before = crate::test_alloc::count();
                let act = self.a.on_input(&mut self.ga, &mut self.na);
                self.alloc_a += crate::test_alloc::count() - before;
                self.ga.after(act, &mut self.a);
                self.gb.do_frame(&mut self.b, &mut self.nb);
            }
        }
    }

    fn lc(peer: u16, nonce: u64, secret: Secret) -> link::Config {
        link::Config {
            handshake: handshake::Config {
                session_id: SID,
                role_nonce: nonce,
                latency: 2,
                name: Name::new("x"),
                host_seat: Seat::P2,
            },
            secret,
            peers: Candidates::from_addrs(&[lo(peer)]).unwrap(),
            key_seed: higanr_core::seal::KeySeed([nonce as u8; 32]),
        }
    }

    #[test]
    fn the_game_side_waits_for_the_slot_and_rebuilds_only_before_connecting() {
        let slot = Arc::new(LinkSlot::new());
        let mut d = Duo::new(&slot);
        d.run(60);
        let st = d.a.code_state();
        assert_eq!((d.a.phase(), st.links), (Phase::Unconnected, 0));
        assert!(st.dropped_before_link > 0, "B の HELLO を捨てていない");
        assert_eq!(d.na.sent, 0, "セッションを作る前に送った");
        assert!(!slot.is_connected());
        slot.offer(lc(2, 0xF000, OTHER_SECRET));
        d.run(60);
        assert_eq!(d.a.code_state().links, 1);
        assert!(d.na.sent > 0, "作ったセッションで送らない");
        assert!(d.a.connected().is_none() && !slot.is_connected());
        slot.offer(lc(2, 0xF000, SECRET));
        d.run(60);
        assert_eq!(d.a.code_state().links, 2);
        assert!(d.a.connected().is_some(), "{:?}", d.a.end());
        assert!(slot.is_connected(), "繋がったことを見張りへ知らせない");
        slot.offer(lc(2, 0xF000, OTHER_SECRET));
        d.run(5);
        assert_eq!(d.a.code_state().links, 2);
        assert!(d.a.connected().is_some());
        d.a.close(ByeReason::Cancelled);
        d.run(2);
        assert!(slot.is_done());
        // セッションを作る前に捨てる・設定を取る・作り直す・繋がる、のどの tick でも確保しない（ゲームのスレッドの経路）。
        assert_eq!(d.alloc_a, 0, "ゲームのスレッドの経路で確保した");
    }

    #[test]
    fn the_game_side_ends_when_the_watcher_gives_up() {
        let slot = Arc::new(LinkSlot::new());
        let mut d = Duo::new(&slot);
        d.run(10);
        assert_eq!(d.a.phase(), Phase::Unconnected);
        slot.give_up();
        d.run(2);
        assert_eq!(
            d.a.end(),
            Some(End::HandshakeFailed(FailReason::NothingReceived))
        );
        assert!(slot.is_done());
    }

    fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
        let t0 = Instant::now();
        while !f() {
            assert!(
                t0.elapsed() < Duration::from_secs(secs),
                "{secs} 秒で {what} にならない"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn quiet() -> String {
        "（検査: 知らせない）".to_string()
    }

    /// ウィンドウで選んだ形を見ない `start`（本物の `start` は最後の引数に `ModeCtx` を取る。ウィンドウの選択を見る検査は `super::start` を直に呼ぶ）。
    fn start(
        s: &NetSettings,
        dir: PathBuf,
        net: &mut GameNet,
        role_nonce: u64,
        slot: Arc<LinkSlot>,
        timing: Timing,
        notify: Notify,
    ) -> Result<(Watch, String), String> {
        super::start(
            s,
            dir,
            net,
            role_nonce,
            slot,
            timing,
            notify,
            ModeCtx::none(),
        )
    }

    // 呼ばれた数は検査ごとに別の static で数える（並んで走る検査と混ざらないように）。
    static PAIR_NOTIFIES: AtomicU32 = AtomicU32::new(0);
    fn pair_notify() -> String {
        PAIR_NOTIFIES.fetch_add(1, Ordering::Relaxed);
        "（検査の知らせ）".to_string()
    }
    static GIVE_UP_NOTIFIES: AtomicU32 = AtomicU32::new(0);
    fn give_up_notify() -> String {
        GIVE_UP_NOTIFIES.fetch_add(1, Ordering::Relaxed);
        "（検査の知らせ）".to_string()
    }

    const FAST: Timing = Timing {
        poll: Duration::from_millis(20),
        window: Duration::from_secs(30),
    };

    #[test]
    #[ignore = "見張りのスレッドを実時間で見る（1.5 秒）。全件の回（build.bat）で --ignored で走る"]
    fn two_offers_are_told_apart_and_the_watchers_give_up() {
        let (da, db) = (temp_dir("both_a"), temp_dir("both_b"));
        let t = Timing {
            poll: Duration::from_millis(20),
            window: Duration::from_millis(1_500),
        };
        let (so_a, so_b) = (settings("offer", "a"), settings("offer", "b"));
        let (mut na, _ra) = net_udp::open_for_code(so_a.bind.fixed().unwrap()).unwrap();
        let (mut nb, _rb) = net_udp::open_for_code(so_b.bind.fixed().unwrap()).unwrap();
        let (sa, sb) = (Arc::new(LinkSlot::new()), Arc::new(LinkSlot::new()));
        let (wa, _) = start(
            &so_a,
            da.clone(),
            &mut na,
            1,
            Arc::clone(&sa),
            t,
            give_up_notify,
        )
        .unwrap();
        let (wb, _) = start(
            &so_b,
            db.clone(),
            &mut nb,
            2,
            Arc::clone(&sb),
            t,
            give_up_notify,
        )
        .unwrap();
        std::fs::copy(da.join(MY_CODE_FILE), db.join(PEER_CODE_FILE)).unwrap();
        std::fs::copy(db.join(MY_CODE_FILE), da.join(PEER_CODE_FILE)).unwrap();
        wait_until("両方の見張りが終わる", 10, || {
            wa.finished() && wb.finished()
        });
        for (w, s, d) in [(&wa, &sa, &da), (&wb, &sb, &db)] {
            let notes = w.notes().join("\n");
            assert!(notes.contains("2 人とも始める側"), "{notes}");
            assert!(notes.contains("ネットワークをやめた"), "{notes}");
            let cf = w.facts();
            assert!(
                cf.refused >= 1 && cf.last_refusal == Some(Refusal::BothOffer),
                "{cf:?}"
            );
            assert!(
                cf.gave_up && cf.peer_read_at.is_none() && cf.connected_at.is_none(),
                "{cf:?}"
            );
            assert_eq!(s.generation(), 0, "読めないのに設定を置いた");
            assert!(s.gave_up());
            assert!(
                !d.join(MY_CODE_FILE).exists(),
                "諦めた後に my_code.txt が残った"
            );
            let status = std::fs::read_to_string(d.join(STATUS_FILE)).unwrap();
            assert!(status.contains("待機をやめました"), "{status}");
            for_people(&status);
            assert!(!notes.contains("検査の知らせ"), "{notes}");
        }
        assert_eq!(
            GIVE_UP_NOTIFIES.load(Ordering::Relaxed),
            0,
            "繋がらずに諦めたのに知らせた"
        );
        drop((wa, wb));
        let _ = std::fs::remove_dir_all(&da);
        let _ = std::fs::remove_dir_all(&db);
    }

    #[test]
    #[ignore = "見張りのスレッドを実時間で起こして止める。全件の回（build.bat）で --ignored で走る"]
    fn dropping_the_watch_removes_the_offer_code() {
        let d = temp_dir("drop");
        let s = settings("offer", "a");
        let (mut n, _r) = net_udp::open_for_code(s.bind.fixed().unwrap()).unwrap();
        let (w, _) = start(
            &s,
            d.clone(),
            &mut n,
            1,
            Arc::new(LinkSlot::new()),
            FAST,
            quiet,
        )
        .unwrap();
        assert!(d.join(MY_CODE_FILE).exists());
        let t0 = Instant::now();
        drop(w);
        assert!(t0.elapsed() < Duration::from_secs(2), "止まらない");
        assert!(!d.join(MY_CODE_FILE).exists(), "my_code.txt が残った");
        let status = std::fs::read_to_string(d.join(STATUS_FILE)).unwrap();
        assert!(status.contains("通信を始められませんでした"), "{status}");
        for_people(&status);
        let _ = std::fs::remove_dir_all(&d);
    }

    fn stun_settings(side: &str, mode: &str, stun: &str) -> NetSettings {
        crate::net_config::from_text(&format!(
            "[netplay]\nenable = 1\nscope = internet\nbind = 127.0.0.1:0\ncode_side = {side}\nname = m\nstun = {stun}\ncandidates = {mode}\n"
        ))
        .unwrap()
        .unwrap()
    }

    #[test]
    fn mapped_candidates_carry_the_address_stun_saw_and_the_local_port_on_it() {
        let addrs = |c: &Code| {
            c.candidates
                .as_slice()
                .iter()
                .map(|x| x.addr)
                .collect::<Vec<_>>()
        };
        let fake = peer_sim::stun_fake::StunFake::spawn_shifted(1).unwrap();
        let stun = fake.addr().to_string();
        let seen_of = |local: SocketAddr| {
            SocketAddr::new(
                local.ip(),
                peer_sim::stun_fake::shifted_port(local.port(), 1),
            )
        };
        let mut offer_secret = None;
        for (mode, tag) in [("all", "cand_all"), ("mapped", "cand_map")] {
            let d = temp_dir(tag);
            let s = stun_settings("offer", mode, &stun);
            let (mut n, _r) = net_udp::open_for_code(s.bind.fixed().unwrap()).unwrap();
            let local = n.local_addr().unwrap();
            let seen = seen_of(local);
            let punch = SocketAddr::new(seen.ip(), local.port());
            assert_ne!(local, seen);
            let (w, _) = start(
                &s,
                d.clone(),
                &mut n,
                1,
                Arc::new(LinkSlot::new()),
                FAST,
                quiet,
            )
            .unwrap();
            let text = std::fs::read_to_string(d.join(MY_CODE_FILE)).unwrap();
            let c = code::read_initiator(text.trim(), net_udp::now_unix()).unwrap();
            let want = if mode == "all" {
                vec![local, seen]
            } else {
                vec![seen, punch]
            };
            assert_eq!(addrs(&c), want, "candidates = {mode}");
            if mode == "mapped" {
                offer_secret = Some((c.secret.unwrap(), c.session_id, text));
            }
            drop(w);
            let _ = std::fs::remove_dir_all(&d);
        }
        let (secret, session, offer_text) = offer_secret.unwrap();
        for (mode, tag) in [("all", "ans_all"), ("mapped", "ans_map")] {
            let d = temp_dir(tag);
            let s = stun_settings("answer", mode, &stun);
            let (mut n, _r) = net_udp::open_for_code(s.bind.fixed().unwrap()).unwrap();
            let local = n.local_addr().unwrap();
            let seen = seen_of(local);
            let punch = SocketAddr::new(seen.ip(), local.port());
            let (w, _) = start(
                &s,
                d.clone(),
                &mut n,
                2,
                Arc::new(LinkSlot::new()),
                FAST,
                quiet,
            )
            .unwrap();
            std::fs::write(d.join(PEER_CODE_FILE), &offer_text).unwrap();
            wait_until("返すコードが書かれる", 10, || {
                d.join(MY_CODE_FILE).exists()
            });
            let reply = std::fs::read_to_string(d.join(MY_CODE_FILE)).unwrap();
            let c = code::read_reply(reply.trim(), &secret, session, net_udp::now_unix()).unwrap();
            let want = if mode == "all" {
                vec![local, seen]
            } else {
                vec![seen, punch]
            };
            assert_eq!(addrs(&c), want, "answer / candidates = {mode}");
            drop(w);
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn mapped_without_a_stun_answer_refuses_to_start() {
        let d = temp_dir("map_none");
        let text = |mode: &str| {
            format!(
                "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = offer\nstun = stun.test.example:3478\ncandidates = {mode}\n"
            )
        };
        let read = |mode: &str| crate::net_config::from_text(&text(mode)).unwrap().unwrap();
        let s = read("mapped");
        let (mut n, _r) = net_udp::open_for_code(s.bind.fixed().unwrap()).unwrap();
        let t0 = Instant::now();
        let e = start(
            &s,
            d.clone(),
            &mut n,
            1,
            Arc::new(LinkSlot::new()),
            FAST,
            quiet,
        )
        .err()
        .expect("答えが無いのに始めた");
        assert!(
            e.contains("candidates = mapped") && e.contains("STUN は使わない"),
            "{e}"
        );
        assert!(t0.elapsed() < Duration::from_secs(1), "待った");
        assert!(
            !d.join(MY_CODE_FILE).exists(),
            "断ったのに my_code.txt を書いた"
        );
        let s = read("all");
        let (mut n, _r2) = net_udp::open_for_code(s.bind.fixed().unwrap()).unwrap();
        let (w, note) = start(
            &s,
            d.clone(),
            &mut n,
            1,
            Arc::new(LinkSlot::new()),
            FAST,
            quiet,
        )
        .unwrap();
        assert!(note.contains("STUN は使わない"), "{note}");
        assert!(d.join(MY_CODE_FILE).exists());
        drop(w);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    #[ignore = "偽のゲーム 2 つを本物の UDP（loopback）と見張りのスレッドで実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn two_dlls_swap_codes_through_files_and_play_over_loopback() {
        let (da, db) = (temp_dir("pair_a"), temp_dir("pair_b"));
        let (so, sa) = (settings("offer", "dll_a"), settings("answer", "dll_b"));
        let (mut na, ra) = net_udp::open_for_code(so.bind.fixed().unwrap()).unwrap();
        let (mut nb, rb) = net_udp::open_for_code(sa.bind.fixed().unwrap()).unwrap();
        let (slot_a, slot_b) = (Arc::new(LinkSlot::new()), Arc::new(LinkSlot::new()));
        let (wa, note_a) = start(
            &so,
            da.clone(),
            &mut na,
            0xF000,
            Arc::clone(&slot_a),
            FAST,
            pair_notify,
        )
        .unwrap();
        let (wb, note_b) = start(
            &sa,
            db.clone(),
            &mut nb,
            0x0F00,
            Arc::clone(&slot_b),
            FAST,
            pair_notify,
        )
        .unwrap();
        assert!(note_a.contains("始める側 offer"), "{note_a}");
        assert!(note_b.contains("返す側 answer"), "{note_b}");
        assert!(
            !db.join(MY_CODE_FILE).exists(),
            "返す側が相手のコードの前に書いた"
        );
        let offer_text = std::fs::read_to_string(da.join(MY_CODE_FILE)).unwrap();
        let cfg = netplay::Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            ..netplay::Config::default()
        };
        let mut npa = NetPlay::with_code_slot(Arc::clone(&slot_a), cfg, &mut na);
        let mut npb = NetPlay::with_code_slot(Arc::clone(&slot_b), cfg, &mut nb);
        const FRAMES: u32 = 90;
        let mut ga = FakeGame::new(0xA, FRAMES, 1, 1);
        let mut gb = FakeGame::new(0xB, FRAMES, 1, 1);
        ga.menu_before_decide = 20;
        gb.menu_before_decide = 20;
        let (mut closed_a, mut closed_b) = (false, false);
        // 人の手: 0 → 返す側の peer_code.txt に始める側のコードの前半だけ（保存の途中）→ 1 → 100 ms 後に全部（BOM・
        // CRLF つき）→ 2 → 返す側の my_code.txt が出たら始める側の peer_code.txt へ → 3。
        let mut hand = 0;
        let mut half_at = Instant::now();
        let mut connected_at = None;
        let mut seats = None;
        let t0 = Instant::now();
        let mut next = t0;
        let mut after_end = 0;
        loop {
            match hand {
                0 => {
                    let half = &offer_text[..offer_text.len() / 2];
                    std::fs::write(db.join(PEER_CODE_FILE), format!("\u{FEFF}{half}")).unwrap();
                    half_at = Instant::now();
                    hand = 1;
                }
                1 if half_at.elapsed() > Duration::from_millis(100) => {
                    std::fs::write(
                        db.join(PEER_CODE_FILE),
                        format!("\u{FEFF}{}\r\n", offer_text.trim()),
                    )
                    .unwrap();
                    hand = 2;
                }
                2 if db.join(MY_CODE_FILE).exists() => {
                    std::fs::copy(db.join(MY_CODE_FILE), da.join(PEER_CODE_FILE)).unwrap();
                    hand = 3;
                }
                _ => {}
            }
            ga.do_frame(&mut npa, &mut na);
            if ga.done && !closed_a {
                npa.close(ByeReason::Finished);
                closed_a = true;
            }
            gb.do_frame(&mut npb, &mut nb);
            if gb.done && !closed_b {
                npb.close(ByeReason::Finished);
                closed_b = true;
            }
            if connected_at.is_none() && npa.connected().is_some() && npb.connected().is_some() {
                connected_at = Some(t0.elapsed());
                seats = npa
                    .connected()
                    .zip(npb.connected())
                    .map(|(a, b)| (a.seat(), b.seat()));
            }
            if npa.phase() == Phase::Ended && npb.phase() == Phase::Ended {
                after_end += 1;
                if after_end > 30 {
                    break;
                }
            }
            assert!(
                t0.elapsed() < Duration::from_secs(30),
                "終わらない: 人の手 {hand} / A {:?} {:?} {:?} / B {:?} {:?} {:?}\nA: {:?}\nB: {:?}",
                npa.phase(),
                npa.end(),
                npa.code_state(),
                npb.phase(),
                npb.end(),
                npb.code_state(),
                wa.notes(),
                wb.notes()
            );
            next += Duration::from_micros(FRAME_US);
            if let Some(d) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
        }
        assert!(connected_at.is_some(), "繋がった時が無い");
        assert_eq!(ga.recs.len(), FRAMES as usize);
        assert_eq!(ga.recs, gb.recs, "両者の試合の列が違う");
        assert_eq!(
            (npa.code_state().links, npb.code_state().links),
            (1, 1),
            "コードから作ったセッションの数"
        );
        wait_until("両方の見張りが終わる", 5, || {
            wa.finished() && wb.finished()
        });
        let (seat_a, seat_b) = seats.expect("繋がった時の席が無い");
        assert_ne!(seat_a, seat_b);
        for (w, d, seat) in [(&wa, &da, seat_a), (&wb, &db, seat_b)] {
            let notes = w.notes().join("\n");
            assert!(notes.contains("繋がった。my_code.txt を消した"), "{notes}");
            assert!(
                notes.contains("対戦の形が決まった: delay 方式（遅れ 3 フレーム）"),
                "決まった形が本ログに無い: {notes}"
            );
            assert!(
                !d.join(MY_CODE_FILE).exists(),
                "繋がった後に my_code.txt が残った"
            );
            let status = std::fs::read_to_string(d.join(STATUS_FILE)).unwrap();
            let line = if seat == Seat::P1 {
                "接続しました。あなたは1Pです。"
            } else {
                "接続しました。あなたは2Pです。"
            };
            assert!(status.contains(line), "自分の席が窓の文に無い: {status}");
            assert!(
                status.contains(&format!("{line}（delay方式・ディレイ 3 フレーム）")),
                "決まった形が窓の文に無い: {status}"
            );
            for_people(&status);
            assert!(d.join(PEER_CODE_FILE).exists());
            let told = notes
                .find("（検査の知らせ）")
                .expect("知らせが本ログに無い");
            let said = notes.find("繋がった。my_code.txt を消した").unwrap();
            assert!(said < told, "窓の文より前に知らせた\n{notes}");
        }
        assert_eq!(
            PAIR_NOTIFIES.load(Ordering::Relaxed),
            2,
            "繋がった知らせの数（2 台で 1 回ずつ）"
        );
        let notes_b = wb.notes().join("\n");
        assert!(notes_b.contains("peer_code.txt を読めない"), "{notes_b}");
        assert!(
            notes_b.contains("返すコードを my_code.txt に書いた"),
            "{notes_b}"
        );
        // 試験のまとめ（本ログの最後の節）: 繋がってマッチを終えた実行は、両側とも ネットワーク・接続コード・繋がったか・選択・マッチ・同期・終わりの理由が 〇 になる。
        for (who, np, w, net) in [("A", &npa, &wa, &na), ("B", &npb, &wb, &nb)] {
            use crate::net_watch::NetStatus;
            use crate::trial_summary::{self as ts, Facts, HostFacts, OpenInfo, Stage, Stun};
            let st = NetStatus::new();
            st.publish(np);
            let facts = Facts {
                game: ts::installed_game_facts(),
                net: Some(st.snapshot()),
                host: HostFacts {
                    open: Some(OpenInfo {
                        local: net.local_addr(),
                        scope: Scope::Loopback,
                        stun: Stun::NotUsed,
                    }),
                    code: Some(w.facts()),
                    ..HostFacts::default()
                },
                pace_end: None,
                stage: Stage::Final,
            };
            let items = ts::judge(&facts);
            for name in [
                ts::N_OPEN,
                ts::N_CODE,
                ts::N_LINK,
                ts::N_SELECT,
                ts::N_MATCH,
                ts::N_SYNC,
                ts::N_END,
            ] {
                let it = ts::item_named(&items, name);
                assert_eq!(it.mark, ts::Mark::Ok, "{who}: {}: {}", it.name, it.text);
            }
            let cf = w.facts();
            assert!(
                cf.peer_read_at.is_some() && cf.connected_at.is_some(),
                "{who}: {cf:?}"
            );
            if who == "B" {
                assert_eq!(cf.refused, 1, "{cf:?}");
                assert_eq!(cf.last_refusal, Some(Refusal::Broken), "{cf:?}");
            }
            eprintln!("試験のまとめ {who}:\n{}", ts::render(&facts));
        }
        let (sa_, sb_) = (ra.stop(), rb.stop());
        drop((wa, wb));
        eprintln!(
            "繋がるまで {connected_at:?}（人の手を含む）/ 受信 A {sa_:?} / B {sb_:?} / 捨てた A {} B {}",
            npa.code_state().dropped_before_link,
            npb.code_state().dropped_before_link
        );
        let _ = std::fs::remove_dir_all(&da);
        let _ = std::fs::remove_dir_all(&db);
    }

    #[test]
    fn a_peer_code_of_another_version_is_refused_by_its_version() {
        const OFFER_V0: &str = "AAFEMyIRANJJawAAAAACYWIAAQIDBAUGBwgJCgsMDQ4PAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBsEJ6ZI";
        let mut ans = answer_swapper(41_009);
        let why = refused(ans.on_peer_file(Some(OFFER_V0.as_bytes()), NOW));
        let want = format!(
            "相手のHiganRToolのバージョンが違います（自分 {}・相手 0）。相手と同じzipのHiganRToolを使ってください。",
            higanr_core::wire::PROTO_VER
        );
        assert!(why.log.contains(&want), "{why:?}");
        assert!(!why.log.contains("字が壊れている"), "{why:?}");
        assert_eq!(why.show, want);
        assert_eq!(why.kind, Kind::Refused(Refusal::OtherVersion { theirs: 0 }));
        assert_eq!(ans.own_text(), None, "読めないのに返すコードを作った");
    }
}
