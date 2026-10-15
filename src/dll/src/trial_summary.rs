//! 試験のまとめ（pure。unsafe 無し）: 試験のあと本ログだけを読んで「どこが上手くいき、どこが上手くいかなかったか」が分かるよう、
//! 項目ごとに 〇（うまくいった）/ ×（うまくいかなかった）/ －（試していない・まだ分からない）と、次にすることが分かる短い理由を 1 行ずつ出す。
//!
//! 集めた値 `Facts`（読み取りのスレッドが作る。ゲームのスレッドには何も足さない）から、判断 `judge` が 14 項目の `Item` を作り、
//! `render` が本ログの節にする。見つけた値だけを使い、値が無ければ －（0 件・100% を「うまくいった」と読まない）。
//!
//! 書き出す時期は `observe::run_len`（1 秒ごとに印の並びが変わったときと 60 秒ごとの「途中経過」）と `session::run` の最後
//! （節の名前は「==== 試験のまとめ ====」）。ゲームを閉じて終えるとプロセスごと消えて終わりの節が書かれないことがあるので、途中経過も残す。
//!
//! 配るバージョン（feature `dist`）でも出る。文言は配るバージョンの走査の語（`tools/make_dist.py` の `INTERNAL_WORDS`）を使わない。
#![forbid(unsafe_code)]

use std::net::SocketAddr;

use higanr_core::guard::Scope;
use higanr_core::net_menu::other_version_text;
use higanr_core::wire::{Check, PROTO_VER};

use crate::net_config::CodeSide;
use crate::net_watch::NetSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Ok,
    Bad,
    Skip,
}

impl Mark {
    pub fn glyph(self) -> &'static str {
        match self {
            Mark::Ok => "〇",
            Mark::Bad => "×",
            Mark::Skip => "－",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub mark: Mark,
    pub name: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdentState {
    #[default]
    NotChecked,
    Match,
    Mismatch,
    Unreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wrote {
    #[default]
    NotTried,
    Written,
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaceKind {
    #[default]
    NotTried,
    Nothing,
    Stamps,
    Paced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pacing {
    pub kind: PaceKind,
    /// ini（か既定）が求めた形と、実際に使う形（始まりを用意できなければ game になる）。
    pub requested: Option<&'static str>,
    pub used: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PaceEnd {
    pub rows: u64,
    pub lost: u64,
    pub fallbacks: u32,
    pub panics: u32,
    pub write_failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Setup {
    #[default]
    NotTried,
    NotConfigured,
    Failed(String),
    On,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GameFacts {
    pub ident: IdentState,
    /// 地点の照合まで進んだか・数・一致しなかった地点の名前。
    pub sites_checked: bool,
    pub sites_total: usize,
    pub sites_matched: usize,
    pub sites_bad: Vec<&'static str>,
    pub input_hook: Wrote,
    pub draw_hook: Wrote,
    pub start_hook: Wrote,
    pub pacing: Pacing,
    pub net: Setup,
    pub replay: Setup,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Stun {
    #[default]
    NotUsed,
    Skipped(String),
    NoAnswer(String),
    Seen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenInfo {
    pub local: Option<SocketAddr>,
    pub scope: Scope,
    pub stun: Stun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Unreadable,
    OwnCode,
    BothOffer,
    BothAnswer,
    Broken,
    Expired,
    WrongSession,
    WrongKind,
    NoRoute,
    Internal,
    /// 相手のコードのバージョンが違う（`theirs` はコードのバージョン、自分のバージョンは `PROTO_VER`）。
    OtherVersion {
        theirs: u8,
    },
    /// 相手のコードのビルド番号が違う（`theirs` はコードの値、自分は `update::BUILD_NUMBER`）。
    OtherBuild {
        theirs: u32,
    },
}

impl Refusal {
    pub fn text(self) -> String {
        if let Refusal::OtherVersion { theirs } = self {
            return other_version_text(PROTO_VER, theirs);
        }
        if let Refusal::OtherBuild { theirs } = self {
            return higanr_core::net_menu::other_build_text(
                higanr_core::update::BUILD_NUMBER,
                theirs,
            );
        }
        match self {
            Refusal::Unreadable => "ファイルを読めなかった（UTF-8 で保存し直して、もう一度貼る）",
            Refusal::OwnCode => "自分のコードを貼っている（相手から届いたコードを貼る）",
            Refusal::BothOffer => {
                "2 人とも始める側（どちらかの ini の code_side を answer にして起動し直す）"
            }
            Refusal::BothAnswer => {
                "2 人とも返す側（どちらかの ini の code_side を offer にして起動し直す）"
            }
            Refusal::Broken => "文字が欠けているか違っている（全部をコピーして貼り直す）",
            Refusal::Expired => "期限が切れている（相手に新しいコードを頼む）",
            Refusal::WrongSession => "前の回の自分のコードへの返事（今のコードを相手へ渡し直す）",
            Refusal::WrongKind => "今の回のコードと合わない（相手に今のコードを頼む）",
            Refusal::NoRoute => {
                "相手のアドレスが、ini の scope の範囲の外（家の中の相手なら lan、外の相手なら internet にする）"
            }
            Refusal::Internal => "準備の中の誤り（ゲームを閉じて起動し直す）",
            Refusal::OtherVersion { .. } | Refusal::OtherBuild { .. } => "",
        }
        .to_string()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodeFacts {
    pub side: CodeSide,
    pub stun: Stun,
    pub peer_read_at: Option<f32>,
    pub refused: u32,
    pub last_refusal: Option<Refusal>,
    pub connected_at: Option<f32>,
    pub ended_unconnected: bool,
    pub gave_up: bool,
}

impl CodeFacts {
    pub fn new(side: CodeSide) -> CodeFacts {
        CodeFacts {
            side,
            stun: Stun::NotUsed,
            peer_read_at: None,
            refused: 0,
            last_refusal: None,
            connected_at: None,
            ended_unconnected: false,
            gave_up: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplayFacts {
    pub attempts: u32,
    pub saved: u32,
    /// 保存関数を呼ばなかった（書き込みが足りない見込み）・呼んだがファイルが 0 バイト・印を足せなかった・保存先の準備ができなかった。
    pub not_called: u32,
    pub empty_file: u32,
    pub mark_failed: u32,
    pub prep_failed: u32,
    pub panicked: bool,
}

impl ReplayFacts {
    /// うまくいかなかった回数。保存先の準備の失敗は、あとで 1 本でも保存できていれば持ち直したので数えない（2 秒ごとに試し直すので続く間は増え続ける）。
    fn failures(&self) -> u32 {
        let prep = if self.saved == 0 { self.prep_failed } else { 0 };
        self.not_called + self.empty_file + self.mark_failed + prep
    }

    fn failure_text(&self) -> String {
        let mut v = Vec::new();
        for (n, why) in [
            (
                self.not_called,
                "保存関数を呼ばなかった（書き込みが足りない見込み）",
            ),
            (self.empty_file, "ゲームが保存先に書かなかった（0 バイト）"),
            (self.mark_failed, "印を足せなかった"),
            (self.prep_failed, "保存先の準備ができなかった"),
        ] {
            if n > 0 {
                v.push(format!("{why} {n} 回"));
            }
        }
        v.join(" / ")
    }
}

/// 読み取りのスレッドが、ゲームの中の静的な値から引く値（ネットワークを開いたときのこと・接続コードの状態・パニック・自動保存）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HostFacts {
    pub open: Option<OpenInfo>,
    pub code: Option<CodeFacts>,
    pub net_panicked: bool,
    pub replay: ReplayFacts,
    /// ゲーム内のメニューの実行（`code_side = game`）で、ネットワークを開く頼みが来たか（メニューを置いていない実行は `NotMenu`）。
    pub menu: MenuNet,
}

/// ゲーム内のメニューの実行の、ネットワークの開き方の段階。メニューは最初の「作る」「貼る」までネットワークを開かないので、その前に終わった回は「ネットワークを使う設定だが、まだ開いていない」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MenuNet {
    #[default]
    NotMenu,
    NotAsked,
    Asked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Interim(u32),
    Final,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub game: GameFacts,
    pub net: Option<NetSnapshot>,
    pub host: HostFacts,
    pub pace_end: Option<PaceEnd>,
    pub stage: Stage,
}

fn menu_unopened(f: &Facts) -> bool {
    f.host.menu == MenuNet::NotAsked && f.host.open.is_none()
}

const MENU_NOT_OPENED: &str = "ゲーム内のメニューで「作る」「貼る」を選ばなかったので開いていない";

/// ネットワークの数の項目の先頭の確かめ: ネットワークを使わない実行・メニューでネットワークを開かなかった実行は、理由つきの － を返す。
fn net_or_skip(f: &Facts, name: &'static str) -> Result<NetSnapshot, Item> {
    if menu_unopened(f) {
        return Err(skip(name, MENU_NOT_OPENED));
    }
    f.net
        .filter(|_| f.game.net == Setup::On)
        .ok_or_else(|| skip(name, "対戦の通信を使っていない"))
}

fn addr_kind(a: SocketAddr) -> &'static str {
    higanr_core::log_scrub::ip_kind(a.ip())
}

fn path_kind(k: u32) -> &'static str {
    match k {
        1 => "同じ PC の中",
        2 => "家の中",
        3 => "外側のアドレス",
        4 => "IPv6",
        _ => "種類は分からない",
    }
}

fn secs(ms: u32) -> String {
    format!("{:.1}", f64::from(ms) / 1000.0)
}

/// ズレの行の末尾の案内。詳細なログには使ったフレームの CSV があるので見比べる。簡潔なログには無いので、詳細で再現するよう案内する。
pub fn compare_frames_hint() -> &'static str {
    if crate::log_mode::detail() {
        "_net_frames.csv.gz の同じ seg・frame の前後を、両者のファイルで見比べる"
    } else {
        "詳しく見るには、ini の [log] の detail を 1 にして再現する"
    }
}

pub fn check_text(c: &Check) -> String {
    format!(
        "seg {} frame {} 乱数の状態 0x{:04X} 抽選回数 {} 1P ライフ {} 2P ライフ {}",
        c.seg, c.frame, c.rng_state, c.rng_count, c.life_1p, c.life_2p
    )
}

pub fn check_diff(a: &Check, b: &Check) -> Vec<&'static str> {
    let mut v = Vec::new();
    for (differs, name) in [
        (a.seg != b.seg, "seg"),
        (a.frame != b.frame, "frame"),
        (a.rng_state != b.rng_state, "乱数の状態"),
        (a.rng_count != b.rng_count, "抽選回数"),
        (a.life_1p != b.life_1p, "1P ライフ"),
        (a.life_2p != b.life_2p, "2P ライフ"),
    ] {
        if differs {
            v.push(name);
        }
    }
    v
}

/// ズレの時点の両者の CHECK を 1 つの文に（本ログの「ネットワーク:」の行と、まとめの「同期」の行が使う）。
pub fn desync_text(local: &Check, remote: &Check) -> String {
    let diff = check_diff(local, remote);
    format!(
        "自分の CHECK: {} / 相手の CHECK: {} / 食い違った欄: {}",
        check_text(local),
        check_text(remote),
        if diff.is_empty() {
            "なし".to_string()
        } else {
            diff.join("・")
        }
    )
}

const FRAME_US: u64 = 16_667;

/// 片道（RTT の半分）が delay フレーム分の時間を超えるか。超えると、相手の入力ワードが使う時刻までに届かず毎フレーム待つはず。
pub fn rtt_exceeds_delay(rtt_ms: u32, delay: u32) -> bool {
    delay > 0 && u64::from(rtt_ms) * 1_000 / 2 > u64::from(delay) * FRAME_US
}

fn item(mark: Mark, name: &'static str, text: impl Into<String>) -> Item {
    Item {
        mark,
        name,
        text: text.into(),
    }
}

fn ok(name: &'static str, text: impl Into<String>) -> Item {
    item(Mark::Ok, name, text)
}
fn bad(name: &'static str, text: impl Into<String>) -> Item {
    item(Mark::Bad, name, text)
}
fn skip(name: &'static str, text: impl Into<String>) -> Item {
    item(Mark::Skip, name, text)
}

pub(crate) const N_VERSION: &str = "花映塚のバージョン";
pub(crate) const N_HOOK: &str = "フックの設置";
pub(crate) const N_PACE: &str = "フレームのペース";
pub(crate) const N_OPEN: &str = "ネットワークを開けたか";
pub(crate) const N_STUN: &str = "STUN の答え";
pub(crate) const N_CODE: &str = "接続コードの受け渡し";
pub(crate) const N_LINK: &str = "繋がったか";
pub(crate) const N_SELECT: &str = "ネットの選択";
pub(crate) const N_MATCH: &str = "試合";
pub(crate) const N_SYNC: &str = "同期";
pub(crate) const N_RTT: &str = "RTT";
pub(crate) const N_END: &str = "終わりの理由";
pub(crate) const N_SAVE: &str = "自動保存";
pub(crate) const N_PANIC: &str = "パニック";

fn version(g: &GameFacts) -> Item {
    match g.ident {
        IdentState::Match => ok(N_VERSION, "花映塚 1.50a と一致した"),
        IdentState::Mismatch => bad(
            N_VERSION,
            "花映塚 1.50a ではなかった（食い違った欄は本ログの「同定:」の行）。何も書き換えていない。1.50a の th09.exe で起動する",
        ),
        IdentState::Unreadable => bad(
            N_VERSION,
            "th09.exe を読めず、バージョンを確かめられなかった（理由は本ログの「同定:」の行）。何も書き換えていない",
        ),
        IdentState::NotChecked => skip(N_VERSION, "まだ確かめていない"),
    }
}

fn hooks(g: &GameFacts) -> Item {
    if g.ident != IdentState::Match {
        return skip(
            N_HOOK,
            "バージョンが確かめられていないので、地点を照合していない",
        );
    }
    if !g.sites_checked {
        return bad(
            N_HOOK,
            "地点の照合まで進まなかった（image base か表の検査が通らなかった。本ログの「image base:」「表の検査」の行）。何も書き換えていない",
        );
    }
    if !g.sites_bad.is_empty() {
        return bad(
            N_HOOK,
            format!(
                "照合が合わなかった地点: {}（{} 地点のうち {} 地点が一致）。何も書き換えていない。ほかの道具（vpatch など）が同じ所を書き換えていないか確かめる",
                g.sites_bad.join("・"),
                g.sites_total,
                g.sites_matched
            ),
        );
    }
    match (g.input_hook, g.start_hook, g.draw_hook) {
        (Wrote::Written, Wrote::Refused, _) => bad(
            N_HOOK,
            format!(
                "{} 地点が全部一致し、入力のフックは書いたが、試合の開始のフックを書けなかったので対戦の通信は開かない（本ログの「フックの設置:」の行）",
                g.sites_total
            ),
        ),
        (Wrote::Written, _, draw) => ok(
            N_HOOK,
            format!(
                "{} 地点が全部一致し、入力のフックを書いた{}",
                g.sites_total,
                match draw {
                    Wrote::Written => "（描画のフックも書いた）",
                    Wrote::Refused => "（描画のフックは書けなかった。描画の記録が空になるだけ）",
                    Wrote::NotTried => "",
                }
            ),
        ),
        (Wrote::Refused, _, _) => bad(
            N_HOOK,
            format!(
                "{} 地点は全部一致したが、入力のフックの書き込みを断った（理由は本ログの「フックの設置:」の行）",
                g.sites_total
            ),
        ),
        (Wrote::NotTried, _, _) => skip(N_HOOK, "地点は全部一致したが、フックはまだ書いていない"),
    }
}

fn pacing(f: &Facts) -> Item {
    let p = &f.game.pacing;
    let end = f.pace_end;
    if let Some(e) = end {
        if e.panics > 0 {
            return bad(
                N_PACE,
                format!(
                    "ペースの先頭でパニックを {} 回受けた（受けた回はゲーム自身の待ちへ流した。本ログの「ペース:」の行）",
                    e.panics
                ),
            );
        }
        if e.write_failed {
            return bad(
                N_PACE,
                "ペースの記録が途中から書けなくなった（ペースそのものは動いた。本ログの「ペース:」の行）",
            );
        }
    }
    let tail = end.map_or(String::new(), |e| {
        format!(
            "。{} フレームを記録、先頭でゲーム自身の経路へ任せた {} 回",
            e.rows, e.fallbacks
        )
    });
    match p.kind {
        PaceKind::NotTried => skip(N_PACE, "この実行では試していない"),
        PaceKind::Nothing => bad(
            N_PACE,
            "ペースのフックを入れなかった。ゲーム自身のペースのまま動く（理由は本ログの「ペース:」の行）",
        ),
        PaceKind::Stamps if p.requested.is_some() && p.requested != p.used => bad(
            N_PACE,
            format!(
                "先頭の待ちを用意できず、要求した {} が使えなかった。ゲーム自身のペースのまま動く（本ログの「ペース:」の行）",
                p.requested.unwrap_or("?")
            ),
        ),
        PaceKind::Stamps => ok(
            N_PACE,
            format!("ゲーム自身のペースのまま、時刻のフックだけ付けた{tail}"),
        ),
        PaceKind::Paced => ok(
            N_PACE,
            format!("{} で刻んでいる{tail}", p.used.unwrap_or("?")),
        ),
    }
}

fn open(f: &Facts) -> Item {
    match &f.game.net {
        Setup::NotTried => skip(N_OPEN, "フックを設置しなかったので、設定を読んでいない"),
        Setup::NotConfigured => skip(N_OPEN, "対戦の通信を使わない設定（ini の [netplay]）"),
        Setup::Failed(why) => bad(
            N_OPEN,
            format!("開けなかった: {why}（本ログの「ネットワーク:」の行）"),
        ),
        Setup::On => match f
            .host
            .open
            .as_ref()
            .and_then(|o| o.local.map(|l| (l, o.scope)))
        {
            Some((local, scope)) => ok(
                N_OPEN,
                format!(
                    "受信ソケットを開いた（受信ソケットのアドレスは{}。scope = {}）",
                    addr_kind(local),
                    scope.name()
                ),
            ),
            None if f.host.menu == MenuNet::NotAsked => skip(N_OPEN, MENU_NOT_OPENED),
            None if f.host.menu == MenuNet::Asked => {
                let why = "ネットワークを開く頼みは来たが、受信ソケットを開けていない（本ログの「ネットワーク:」の行にその理由がある。メニューからやり直せる）";
                if f.stage == Stage::Final {
                    bad(N_OPEN, why)
                } else {
                    skip(N_OPEN, why)
                }
            }
            None => ok(
                N_OPEN,
                "受信ソケットを開いた（受信ソケットのアドレスは取れていない）",
            ),
        },
    }
}

fn stun(f: &Facts) -> Item {
    if menu_unopened(f) {
        return skip(N_STUN, MENU_NOT_OPENED);
    }
    if f.game.net != Setup::On {
        return skip(N_STUN, "対戦の通信を開いていない");
    }
    let s = f
        .host
        .open
        .as_ref()
        .map(|o| &o.stun)
        .or(f.host.code.as_ref().map(|c| &c.stun));
    match s {
        None => skip(N_STUN, "結果を取れていない"),
        Some(Stun::NotUsed) => skip(
            N_STUN,
            "使っていない（ini に stun の指定が無い。同じ家の中の相手だけなら要らない）",
        ),
        // 外側のポートが受信ソケットと同じか・待つ間の聞き直しの数（アドレス・ポートの値は書かない）。診断が無い実行は 1 句のまま。
        Some(Stun::Seen) => {
            let tail = f
                .net
                .filter(|s| s.diag.port_kept.is_some() || s.diag.keep.installed)
                .map_or(String::new(), |s| {
                    let second = if s.diag.second == crate::net_udp::SecondStun::NotAsked {
                        String::new()
                    } else {
                        format!("。{}", s.diag.second.text())
                    };
                    format!("（{}。{}{second}）", s.diag.port_text(), s.diag.keep_text())
                });
            ok(N_STUN, format!("外側のアドレスあり{tail}"))
        }
        Some(Stun::Skipped(why)) => bad(
            N_STUN,
            format!("問い合わせなかった: {why}（外の相手と繋ぐには外側のアドレスが要る）"),
        ),
        Some(Stun::NoAnswer(why)) => bad(
            N_STUN,
            format!(
                "答えが返らなかった: {why}（外側のアドレスが分からない。ネットの接続を確かめる。同じ家の中の相手だけなら要らない）"
            ),
        ),
    }
}

fn code(f: &Facts) -> Item {
    if menu_unopened(f) {
        return skip(N_CODE, MENU_NOT_OPENED);
    }
    let Some(c) = &f.host.code else {
        return skip(N_CODE, "接続コードを使わない設定");
    };
    let net_ended = f.net.is_some_and(|s| s.link_ended());
    let final_stage = f.stage == Stage::Final;
    let side = match c.side {
        CodeSide::Offer => "始める側",
        CodeSide::Answer => "返す側",
        CodeSide::Spectate => "見るだけの側",
        CodeSide::Game => "ゲーム内のメニュー",
    };
    if let Some(t) = c.peer_read_at {
        let again = match (c.refused, c.last_refusal) {
            (n, Some(r)) if n > 0 => format!(
                "。その前後に読めなかったことが {n} 回あった（最後は: {}）",
                r.text()
            ),
            _ => String::new(),
        };
        return ok(
            N_CODE,
            format!("{side}。相手のコードを読めた（開いてから {t:.1} 秒）{again}"),
        );
    }
    if let Some(r) = c.last_refusal {
        return bad(
            N_CODE,
            format!(
                "{side}。相手のコードを読めなかった（{} 回）: {}",
                c.refused,
                r.text()
            ),
        );
    }
    // 次にすること: ゲーム内のメニューは起動し直さずにメニューからつなぎ直せる。ローダーのウィンドウの形は起動し直す。
    let again = if c.side == CodeSide::Game {
        "「妖怪 対 妖怪」のメニューからもう一度つなぐ"
    } else {
        "相手のコードを貼ってから起動し直す"
    };
    if c.gave_up {
        return bad(
            N_CODE,
            format!("{side}。使えるコードが貼られないまま待つのをやめた（{again}）"),
        );
    }
    if c.ended_unconnected || (final_stage && net_ended) {
        return bad(
            N_CODE,
            format!("{side}。相手のコードが貼られないまま終わった（{again}）"),
        );
    }
    skip(
        N_CODE,
        format!("{side}。相手のコードが貼られるのを待っている"),
    )
}

fn link_failure(s: &NetSnapshot) -> String {
    if s.end == crate::net_watch::END_OTHER_VERSION {
        return other_version_text(PROTO_VER, u8::try_from(s.end_detail).unwrap_or(u8::MAX));
    }
    if s.end == crate::net_watch::END_OTHER_BUILD {
        return higanr_core::net_menu::other_build_text(
            higanr_core::update::BUILD_NUMBER,
            s.end_detail,
        );
    }
    let why = match (s.end, s.end_detail) {
        (1, 0) => "相手の HELLO が自分と同じ役の値だった（自分の受信ソケットへ送り返している）",
        (1, 1) => "10 秒のあいだ相手から 1 通も届かなかった",
        (1, 2) => "相手から届いたが、10 秒で繋がらなかった",
        _ => "繋がる前に終わった",
    };
    let hint = if s.recv_in == 0 {
        "相手から 1 通も届いていない。相手のコードのアドレスが合っているか、相手が起動しているか、ファイアウォールとルーターを確かめる"
    } else if s.recv_bad_tag > 0 {
        "タグの誤りが届いている。2 人のコードが揃っていない（コードを取り違えていないか）"
    } else if s.recv_wrong_session > 0 {
        "別のセッションの通信が届いている。前の回のコードで繋いでいないか"
    } else if s.recv_foreign > 0 {
        "候補に無い送り元から届いている。相手の外側のアドレスが変わったか、コードの候補に載っていない（相手のログも見る）"
    } else {
        "届いているが繋がらない。片方向だけ通っている見込み（相手のログも見る）"
    };
    let last = s
        .last_foreign_text()
        .map_or(String::new(), |a| format!("、最後は {a}"));
    format!(
        "{why}。作り直し {} 回、受信ソケットに届いた {} 通（候補に無い送り元 {} 通{last} / タグの誤り {} 通 / 別のセッション {} 通）。{hint}",
        s.hs_restarts, s.recv_in, s.recv_foreign, s.recv_bad_tag, s.recv_wrong_session
    )
}

fn link(f: &Facts) -> Item {
    let s = match net_or_skip(f, N_LINK) {
        Ok(s) => s,
        Err(item) => return item,
    };
    // 繋がらなかった理由を読むための診断（送った HELLO の数・候補の数・コードの外側のアドレスの古さ・聞き直し）。セッションを作った後だけ付ける
    // （作る前は数が全部 0 で、0 件と測っていないが見分けられない）。
    let diag = if s.has_link_box() {
        format!("。{}", s.diag_text())
    } else {
        String::new()
    };
    if s.connected {
        let learned = if s.path_learned {
            "（コードに無い送り元を学んだアドレス）"
        } else {
            ""
        };
        return ok(
            N_LINK,
            format!(
                "繋がった（接続の手順を始めてから {} 秒、経路は{}{learned}、作り直し {} 回）{diag}",
                secs(s.connect_wait_ms),
                path_kind(s.path_kind),
                s.hs_restarts
            ),
        );
    }
    if s.link_ended() {
        return bad(
            N_LINK,
            format!("繋がらずに終わった: {}{diag}", link_failure(&s)),
        );
    }
    if f.stage == Stage::Final {
        return bad(
            N_LINK,
            format!("繋がらないまま計測を終えた（{}）{diag}", link_failure(&s)),
        );
    }
    if s.hs_restarts > 0 {
        return skip(
            N_LINK,
            format!(
                "まだ繋がっていない（作り直し {} 回。{}）{diag}",
                s.hs_restarts,
                link_failure(&s)
            ),
        );
    }
    skip(N_LINK, format!("まだ繋がっていない{diag}"))
}

fn select(f: &Facts) -> Item {
    let s = match net_or_skip(f, N_SELECT) {
        Ok(s) => s,
        Err(item) => return item,
    };
    let pre = format!("繋がる前に落とした決定 {} 回", s.menu_pre_link_decides);
    if !s.connected {
        return skip(N_SELECT, format!("繋がっていないので試していない（{pre}）"));
    }
    match s.end {
        7 => {
            return bad(
                N_SELECT,
                format!(
                    "届いた押下を入れられず、両者の画面が分かれた（押下の step {}）。選択の画面から入り直す",
                    s.end_detail
                ),
            );
        }
        6 => {
            return bad(
                N_SELECT,
                format!("相手の SELECT の値が範囲の外だった（値 {}）", s.end_detail),
            );
        }
        _ => {}
    }
    if let Some(flags) = s.agreed_mismatch.filter(|f| *f != 0) {
        let mut v = Vec::new();
        for (bit, name) in [
            (1, "マップ"),
            (2, "難易度"),
            (4, "オプションのダイジェスト"),
        ] {
            if flags & bit != 0 {
                v.push(name);
            }
        }
        return bad(
            N_SELECT,
            format!(
                "揃った値が両者で食い違った: {}（{pre}）。2 人の設定を揃えて、選択からやり直す",
                v.join("・")
            ),
        );
    }
    if s.menu_align_miss > 0 || s.menu_lost > 0 {
        return bad(
            N_SELECT,
            format!(
                "カーソルを合わせられなかった {} 回 / 押下が入らずに選択を出た {} 回（入った押下 {}。{pre}）",
                s.menu_align_miss, s.menu_lost, s.menu_applied
            ),
        );
    }
    // 押下が入ったか、SELECT が両者で揃った（揃った値がある）なら選択は通っている（選択の画面を持たない偽のゲームは押下が 0 のまま SELECT だけが揃う）。
    if s.menu_applied > 0 || s.agreed_mismatch.is_some() {
        return ok(
            N_SELECT,
            format!(
                "入った押下 {}、SELECT の揃った値 {}、食い違い 0、{pre}",
                s.menu_applied,
                if s.agreed_mismatch.is_some() {
                    "あり"
                } else {
                    "まだ無い"
                }
            ),
        );
    }
    skip(
        N_SELECT,
        format!("まだ選択の押下も SELECT の揃った値も無い（{pre}）"),
    )
}

fn game_match(f: &Facts) -> Item {
    let s = match net_or_skip(f, N_MATCH) {
        Ok(s) => s,
        Err(item) => return item,
    };
    if !s.connected {
        return skip(N_MATCH, "繋がっていないので試していない");
    }
    if s.segs_started > 0 {
        let base = format!(
            "始まった試合 {}、使ったフレーム {}、stall {}（最長の連続 {}）、CHECK の一致 {} 回",
            s.segs_started, s.played, s.stalls, s.max_stall_run, s.check_matches
        );
        if s.charge_write_failures > 0 {
            return bad(
                N_MATCH,
                format!(
                    "{base}。チャージ型を書けなかった {} 回",
                    s.charge_write_failures
                ),
            );
        }
        return ok(N_MATCH, base);
    }
    if s.link_ended() {
        return bad(
            N_MATCH,
            "試合が始まらないままネットワークが終わった（終わりの理由は下の行）",
        );
    }
    skip(N_MATCH, "まだ試合が始まっていない")
}

fn sync(f: &Facts) -> Item {
    let s = match net_or_skip(f, N_SYNC) {
        Ok(s) => s,
        Err(item) => return item,
    };
    // マッチの最中のズレはマッチを止める（`NetPlay::stop_flags`）。始まりの「ズレ（試合を止めた）」で止めたかどうかを先に言う。
    let stopped = s.stop & 1 != 0;
    if let Some((local, remote)) = s.desync {
        return bad(
            N_SYNC,
            format!(
                "{}CHECK がずれた（seg {} frame {}）。{}。{}",
                if stopped {
                    "ズレ（試合を止めた）。"
                } else {
                    ""
                },
                local.seg,
                local.frame,
                desync_text(&local, &remote),
                compare_frames_hint()
            ),
        );
    }
    if stopped {
        return bad(
            N_SYNC,
            format!(
                "ズレ（試合を止めた）。相手が CHECK のズレを見つけて知らせてきた（両者の CHECK の値は相手のログにある）。{}",
                compare_frames_hint()
            ),
        );
    }
    if !s.connected || s.segs_started == 0 {
        return skip(N_SYNC, "試合がまだ始まっていないので比べていない");
    }
    if s.check_matches > 0 {
        return ok(
            N_SYNC,
            format!("ズレ 0（CHECK が {} 回一致）", s.check_matches),
        );
    }
    skip(
        N_SYNC,
        format!(
            "CHECK をまだ 1 回も比べていない（使ったフレーム {}）",
            s.played
        ),
    )
}

fn rtt(f: &Facts) -> Item {
    let s = match net_or_skip(f, N_RTT) {
        Ok(s) => s,
        Err(item) => return item,
    };
    match s.rtt_ms {
        // rollback は待たずに予測する。RTT の大きさだけでは × にせず、片道が D を超えるときは理由を添えて － にする。
        Some(ms) if s.rollback && rtt_exceeds_delay(ms, s.delay) => skip(
            N_RTT,
            format!(
                "{ms} ms。rollback（入力の遅れ D {}）では待たずに相手の入力を予測する。片道が D の時間を超える分だけ予測が外れたときの飛びが増えうる",
                s.delay
            ),
        ),
        Some(ms) if s.rollback => ok(
            N_RTT,
            format!(
                "{ms} ms（最後に測った値。rollback・入力の遅れ D {}）",
                s.delay
            ),
        ),
        Some(ms) if rtt_exceeds_delay(ms, s.delay) => bad(
            N_RTT,
            format!(
                "{ms} ms。片道が delay {} フレームの時間を超えるので、毎フレーム相手の入力を待つ見込み。近い相手と試すか、遅延を増やす",
                s.delay
            ),
        ),
        Some(ms) => ok(
            N_RTT,
            format!("{ms} ms（最後に測った値。delay {} フレーム）", s.delay),
        ),
        None if s.connected => skip(N_RTT, "まだ測れていない"),
        None => skip(N_RTT, "繋がっていない"),
    }
}

fn ending(f: &Facts) -> Item {
    let s = match net_or_skip(f, N_END) {
        Ok(s) => s,
        Err(item) => return item,
    };
    let d = s.end_detail;
    match s.end {
        0 => skip(N_END, "ネットワークはまだ終わっていない"),
        1 => bad(N_END, "接続できなかった（理由は「繋がったか」の行）"),
        2 => match d {
            1 => ok(N_END, "相手が試合を終えた（相手からの BYE・終了）"),
            2 => bad(
                N_END,
                "相手が切断した（相手からの BYE・切断。相手のログを見る）",
            ),
            3 => bad(
                N_END,
                "相手が CHECK のズレを見つけた（相手からの BYE・ズレ。相手のログを見る）",
            ),
            4 => bad(
                N_END,
                "相手が中止した（相手からの BYE・中止。相手のログを見る）",
            ),
            _ => bad(N_END, format!("相手から BYE を受けた（理由の番号 {d}）")),
        },
        3 => bad(
            N_END,
            "相手が黙った（一定時間、相手から何も届かなかった。相手が閉じた・回線が切れた・ファイアウォール。相手のログを見る）",
        ),
        4 => bad(
            N_END,
            format!("CHECK のズレ（frame {d}）。詳しくは「同期」の行"),
        ),
        5 => match d {
            1 => ok(N_END, "自分から終えた"),
            _ => bad(N_END, format!("自分から中止した（理由の番号 {d}）")),
        },
        6 => bad(
            N_END,
            "相手の SELECT が範囲の外だった。詳しくは「ネットの選択」の行",
        ),
        7 => bad(
            N_END,
            "押下を持ち越しすぎて画面が分かれた。詳しくは「ネットの選択」の行",
        ),
        // 道具の不具合ではない（どちらかが「妖怪 対 妖怪」を選ばなかった）ので × にしない。
        8 => skip(
            N_END,
            "つながったが、対戦が成立しないまま時間切れで閉じた（どちらかが「妖怪 対 妖怪」を選ばなかった）",
        ),
        // 8 と同じく道具の不具合ではない（相手の使い方・バージョンの違い）。
        9 => skip(
            N_END,
            "つながったが、相手の HiganRTool が古いか、相手がゲーム内のメニューからつないでいないので閉じた（相手の READY が来ないまま選択の通信が来た）",
        ),
        10 => skip(
            N_END,
            "つながったが、対戦が成立しないまま相手が閉じた（相手の時間切れか、相手が抜けた。相手のログを見る）",
        ),
        // 相手の HiganRTool のバージョンが違う（`net_watch::END_OTHER_VERSION`）。道具の使い方で直る（同じ zip を使う）が、マッチはできなかった。
        11 => bad(
            N_END,
            other_version_text(PROTO_VER, u8::try_from(d).unwrap_or(u8::MAX)),
        ),
        12 => bad(
            N_END,
            higanr_core::net_menu::other_build_text(higanr_core::update::BUILD_NUMBER, d),
        ),
        _ => bad(N_END, format!("終わりの種類が読めない（{}）", s.end)),
    }
}

fn autosave(f: &Facts) -> Item {
    match &f.game.replay {
        Setup::NotTried => skip(N_SAVE, "この実行では設定を読んでいない"),
        Setup::NotConfigured => skip(N_SAVE, "入れていない設定（ini の [replay]）"),
        Setup::Failed(why) => bad(N_SAVE, format!("入れられなかった: {why}")),
        Setup::On => {
            let r = &f.host.replay;
            if r.panicked {
                return bad(
                    N_SAVE,
                    format!("パニックを受けて止めた（保存できたのは {} 本）", r.saved),
                );
            }
            if r.failures() > 0 {
                return bad(
                    N_SAVE,
                    format!(
                        "保存できたのは {} 本、うまくいかなかったのは {} 回: {}",
                        r.saved,
                        r.failures(),
                        r.failure_text()
                    ),
                );
            }
            if r.saved > 0 {
                return ok(N_SAVE, format!("{} 本保存した", r.saved));
            }
            skip(
                N_SAVE,
                format!("試合の終わりをまだ見ていない（試みた {} 回）", r.attempts),
            )
        }
    }
}

fn panics(f: &Facts) -> Item {
    if f.game.input_hook != Wrote::Written {
        return skip(N_PANIC, "フックを入れていないので見ていない");
    }
    let mut v = Vec::new();
    if f.host.net_panicked {
        v.push("対戦の通信の入口".to_string());
    }
    if f.host.replay.panicked {
        v.push("自動保存".to_string());
    }
    if let Some(e) = f.pace_end.filter(|e| e.panics > 0) {
        v.push(format!("ペースの先頭（{} 回）", e.panics));
    }
    if v.is_empty() {
        ok(N_PANIC, "受けていない")
    } else {
        bad(
            N_PANIC,
            format!(
                "{} でパニックを受けて、その機能を止めた（ゲームは元の動きで続く）。本ログのその行と、この本ログを送る",
                v.join("・")
            ),
        )
    }
}

pub fn judge(f: &Facts) -> Vec<Item> {
    vec![
        version(&f.game),
        hooks(&f.game),
        pacing(f),
        open(f),
        stun(f),
        code(f),
        link(f),
        select(f),
        game_match(f),
        sync(f),
        rtt(f),
        ending(f),
        autosave(f),
        panics(f),
    ]
}

#[cfg(test)]
pub fn marks(items: &[Item]) -> Vec<Mark> {
    items.iter().map(|i| i.mark).collect()
}

/// RTT 以外の印の並び（変わったときだけ途中経過を書くための比べ用）。rollback の RTT は片道が D の時間をまたいで 〇 ⇄ － と揺れて変わりの大半を占めた
/// （1 起動で 2,319 回中 2,240 回）ため、比べから外す。外す項目は名（`N_RTT`）で決める。
pub fn marks_without_rtt(items: &[Item]) -> Vec<Mark> {
    items
        .iter()
        .filter(|i| i.name != N_RTT)
        .map(|i| i.mark)
        .collect()
}

/// 簡潔なログの途中経過: 前に書いた項目から印が変わった項目だけ。`prev` が無い（最初の）ときは 〇 と × の項目（－ は書かない）。
/// RTT は 〇 と － の間を行き来するので見ない（`marks_without_rtt` と同じ）。
pub fn render_changes(k: u32, items: &[Item], prev: Option<&[Item]>) -> String {
    let mut out = format!("==== 状態の変化（計測 {k} 秒の時点） ====\n");
    for (i, it) in items.iter().enumerate() {
        if it.name == N_RTT {
            continue;
        }
        let changed = match prev {
            None => it.mark != Mark::Skip,
            Some(p) => p.get(i).map(|x| x.mark) != Some(it.mark),
        };
        if changed {
            out.push_str(&format!("{} {}: {}\n", it.mark.glyph(), it.name, it.text));
        }
    }
    out
}

pub fn render(f: &Facts) -> String {
    let head = match f.stage {
        Stage::Final => "==== 試験のまとめ ====".to_string(),
        Stage::Interim(k) => format!("==== 試験のまとめ（途中経過。計測 {k} 秒の時点） ===="),
    };
    let mut out = format!("{head}\n");
    for i in judge(f) {
        out.push_str(&format!("{} {}: {}\n", i.mark.glyph(), i.name, i.text));
    }
    out
}

/// 検査用: 設置が全部うまくいき、ネットワークと自動保存を入れた実行の、設置までの材料。
#[cfg(test)]
pub(crate) fn installed_game_facts() -> GameFacts {
    GameFacts {
        ident: IdentState::Match,
        sites_checked: true,
        sites_total: 20,
        sites_matched: 20,
        sites_bad: vec![],
        input_hook: Wrote::Written,
        draw_hook: Wrote::Written,
        start_hook: Wrote::Written,
        pacing: Pacing {
            kind: PaceKind::Paced,
            requested: Some("auto"),
            used: Some("auto"),
        },
        net: Setup::On,
        replay: Setup::On,
    }
}

#[cfg(test)]
pub(crate) fn item_named(items: &[Item], name: &str) -> Item {
    items
        .iter()
        .find(|i| i.name == name)
        .unwrap_or_else(|| panic!("項目 {name} が無い"))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chk(seg: u16, frame: u32, rng: u16) -> Check {
        Check {
            seg,
            frame,
            rng_state: rng,
            rng_count: 77,
            life_1p: 3,
            life_2p: 2,
        }
    }

    fn lo() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 41_001))
    }

    fn good() -> Facts {
        Facts {
            game: installed_game_facts(),
            net: Some(NetSnapshot {
                phase: 5,
                connected: true,
                path_kind: 2,
                connect_wait_ms: 2_300,
                segs_started: 2,
                played: 3_600,
                stalls: 4,
                max_stall_run: 2,
                check_matches: 12,
                menu_applied: 6,
                menu_pre_link_decides: 1,
                delay: 3,
                rtt_ms: Some(20),
                end: 2,
                end_detail: 1,
                ..NetSnapshot::default()
            }),
            host: HostFacts {
                open: Some(OpenInfo {
                    local: Some(SocketAddr::from(([192, 168, 0, 5], 41_001))),
                    scope: Scope::Lan,
                    stun: Stun::Seen,
                }),
                code: Some(CodeFacts {
                    peer_read_at: Some(31.2),
                    connected_at: Some(33.5),
                    ..CodeFacts::new(CodeSide::Offer)
                }),
                net_panicked: false,
                replay: ReplayFacts {
                    attempts: 2,
                    saved: 2,
                    ..ReplayFacts::default()
                },
                menu: MenuNet::NotMenu,
            },
            pace_end: Some(PaceEnd {
                rows: 7_000,
                ..PaceEnd::default()
            }),
            stage: Stage::Final,
        }
    }

    fn mark_of(f: &Facts, name: &str) -> Mark {
        judge(f)
            .into_iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("項目 {name} が無い"))
            .mark
    }

    fn text_of(f: &Facts, name: &str) -> String {
        judge(f).into_iter().find(|i| i.name == name).unwrap().text
    }

    fn net_mut(f: &mut Facts) -> &mut NetSnapshot {
        f.net.as_mut().unwrap()
    }

    #[test]
    fn a_good_run_is_all_ok_in_a_fixed_order() {
        let f = good();
        let items = judge(&f);
        let names: Vec<_> = items.iter().map(|i| i.name).collect();
        assert_eq!(
            names,
            [
                N_VERSION, N_HOOK, N_PACE, N_OPEN, N_STUN, N_CODE, N_LINK, N_SELECT, N_MATCH,
                N_SYNC, N_RTT, N_END, N_SAVE, N_PANIC
            ]
        );
        for i in &items {
            assert_eq!(i.mark, Mark::Ok, "{}: {}", i.name, i.text);
        }
        let text = render(&f);
        assert!(text.starts_with("==== 試験のまとめ ====\n"), "{text}");
        assert_eq!(text.lines().count(), 15, "{text}");
        assert!(text.lines().skip(1).all(|l| l.starts_with("〇 ")), "{text}");
        let interim = render(&Facts {
            stage: Stage::Interim(120),
            ..good()
        });
        assert!(
            interim.starts_with("==== 試験のまとめ（途中経過。計測 120 秒の時点） ====\n"),
            "{interim}"
        );
    }

    #[test]
    fn version_marks() {
        let mut f = good();
        assert_eq!(mark_of(&f, N_VERSION), Mark::Ok);
        f.game.ident = IdentState::Mismatch;
        assert_eq!(mark_of(&f, N_VERSION), Mark::Bad);
        assert!(text_of(&f, N_VERSION).contains("1.50a ではなかった"));
        f.game.ident = IdentState::Unreadable;
        assert_eq!(mark_of(&f, N_VERSION), Mark::Bad);
        f.game.ident = IdentState::NotChecked;
        assert_eq!(mark_of(&f, N_VERSION), Mark::Skip);
    }

    #[test]
    fn hook_marks_name_the_bad_site() {
        let mut f = good();
        assert_eq!(mark_of(&f, N_HOOK), Mark::Ok);
        f.game.sites_bad = vec!["feed_call", "match_start"];
        f.game.sites_matched = 18;
        assert_eq!(mark_of(&f, N_HOOK), Mark::Bad);
        let t = text_of(&f, N_HOOK);
        assert!(
            t.contains("feed_call・match_start") && t.contains("20 地点のうち 18"),
            "{t}"
        );
        let mut f = good();
        f.game.input_hook = Wrote::Refused;
        assert_eq!(mark_of(&f, N_HOOK), Mark::Bad);
        let mut f = good();
        f.game.start_hook = Wrote::Refused;
        assert_eq!(mark_of(&f, N_HOOK), Mark::Bad);
        let mut f = good();
        f.game.draw_hook = Wrote::Refused;
        assert_eq!(
            mark_of(&f, N_HOOK),
            Mark::Ok,
            "描画のフックだけ書けなくても計測は続く"
        );
        let mut f = good();
        f.game.sites_checked = false;
        assert_eq!(mark_of(&f, N_HOOK), Mark::Bad);
        let mut f = good();
        f.game.ident = IdentState::Mismatch;
        assert_eq!(mark_of(&f, N_HOOK), Mark::Skip);
        let mut f = good();
        f.game.input_hook = Wrote::NotTried;
        assert_eq!(mark_of(&f, N_HOOK), Mark::Skip);
    }

    #[test]
    fn pacing_marks() {
        let mut f = good();
        assert_eq!(mark_of(&f, N_PACE), Mark::Ok);
        assert!(text_of(&f, N_PACE).contains("auto で刻んでいる"));
        f.game.pacing.kind = PaceKind::Nothing;
        assert_eq!(mark_of(&f, N_PACE), Mark::Bad);
        f.game.pacing.kind = PaceKind::NotTried;
        assert_eq!(mark_of(&f, N_PACE), Mark::Skip);
        let mut f = good();
        f.game.pacing = Pacing {
            kind: PaceKind::Stamps,
            requested: Some("auto"),
            used: Some("game"),
        };
        assert_eq!(mark_of(&f, N_PACE), Mark::Bad, "要求した形が使えなかった");
        f.game.pacing.requested = Some("game");
        assert_eq!(mark_of(&f, N_PACE), Mark::Ok, "game を選んだ設定");
        let mut f = good();
        f.pace_end = Some(PaceEnd {
            panics: 2,
            ..PaceEnd::default()
        });
        assert_eq!(mark_of(&f, N_PACE), Mark::Bad);
        f.pace_end = Some(PaceEnd {
            write_failed: true,
            ..PaceEnd::default()
        });
        assert_eq!(mark_of(&f, N_PACE), Mark::Bad);
    }

    #[test]
    fn open_marks_and_address_kinds() {
        let mut f = good();
        assert_eq!(mark_of(&f, N_OPEN), Mark::Ok);
        assert!(
            text_of(&f, N_OPEN).contains("家の中"),
            "{}",
            text_of(&f, N_OPEN)
        );
        for (addr, word) in [
            (lo(), "同じ PC の中"),
            (SocketAddr::from(([203, 0, 113, 9], 4_000)), "外"),
        ] {
            f.host.open.as_mut().unwrap().local = Some(addr);
            assert!(
                text_of(&f, N_OPEN).contains(&format!("受信ソケットのアドレスは{word}")),
                "{addr}"
            );
        }
        f.game.net = Setup::Failed("受信ソケットを開けない（10049）".to_string());
        assert_eq!(mark_of(&f, N_OPEN), Mark::Bad);
        assert!(text_of(&f, N_OPEN).contains("10049"));
        f.game.net = Setup::NotConfigured;
        assert_eq!(mark_of(&f, N_OPEN), Mark::Skip);
    }

    #[test]
    fn stun_marks() {
        let mut f = good();
        assert_eq!(mark_of(&f, N_STUN), Mark::Ok);
        assert_eq!(text_of(&f, N_STUN), "外側のアドレスあり");
        f.host.open.as_mut().unwrap().stun = Stun::NoAnswer("STUN の答えが返らない".to_string());
        assert_eq!(mark_of(&f, N_STUN), Mark::Bad);
        f.host.open.as_mut().unwrap().stun = Stun::Skipped("名前を引けない".to_string());
        assert_eq!(mark_of(&f, N_STUN), Mark::Bad);
        f.host.open.as_mut().unwrap().stun = Stun::NotUsed;
        assert_eq!(mark_of(&f, N_STUN), Mark::Skip);
        f.host.open = None;
        f.host.code.as_mut().unwrap().stun = Stun::Seen;
        assert_eq!(mark_of(&f, N_STUN), Mark::Ok);
        f.game.net = Setup::NotConfigured;
        assert_eq!(mark_of(&f, N_STUN), Mark::Skip);
    }

    #[test]
    fn link_and_stun_rows_carry_the_diagnostics_only_after_a_box_exists() {
        use crate::net_udp::{KeepDiag, NetDiag};
        let diag = NetDiag {
            port_kept: Some(false),
            stun_at_ms: Some(2_000),
            keep: KeepDiag {
                installed: true,
                running: false,
                counts: higanr_core::stun::KeepCounts {
                    sent: 12,
                    answered: 11,
                    changed_port: 1,
                    differs_from_code: true,
                    ..Default::default()
                },
                send_errors: 0,
            },
            ..NetDiag::default()
        };
        let f = good();
        assert_eq!(text_of(&f, N_STUN), "外側のアドレスあり");
        assert!(
            !text_of(&f, N_LINK).contains("診断"),
            "{}",
            text_of(&f, N_LINK)
        );
        let (mark_link, mark_stun) = (mark_of(&f, N_LINK), mark_of(&f, N_STUN));
        let mut g = good();
        assert!(
            !text_of(&g, N_LINK).contains("学んだアドレス"),
            "{}",
            text_of(&g, N_LINK)
        );
        net_mut(&mut g).path_learned = true;
        let t = text_of(&g, N_LINK);
        assert!(
            t.starts_with("繋がった（接続の手順を始めてから 2.3 秒、経路は")
                && t.contains("（コードに無い送り元を学んだアドレス）、作り直し"),
            "{t}"
        );
        let mut f = good();
        {
            let s = net_mut(&mut f);
            s.peer_cands = [0, 0, 1, 0];
            s.hello_sent = [0, 0, 41, 0];
            s.link_started_ms = 176_000;
            s.diag = diag;
        }
        let t = text_of(&f, N_LINK);
        assert!(
            t.starts_with("繋がった（接続の手順を始めてから 2.3 秒")
                && t.contains("送った HELLO [同じ PC の中 0 / 家の中 0 / 外側 41 / IPv6 0]")
                && t.contains("接続の手順を始めるまで: 174.0 秒"),
            "{t}"
        );
        let t = text_of(&f, N_STUN);
        assert!(
            t.starts_with("外側のアドレスあり（外側のポートは受信ソケットのポートと違う")
                && t.contains("外側のアドレスが変わった 1 回（ポートだけ 1 / アドレスも 0）"),
            "{t}"
        );
        assert!(!t.contains("2 か所目の STUN"), "{t}");
        net_mut(&mut f).diag.second =
            crate::net_udp::SecondStun::Compared(higanr_core::stun::Mapping::PortOnly);
        let t = text_of(&f, N_STUN);
        assert!(
            t.contains("2 か所目の STUN: 外側のポートだけ 1 か所目と違う") && t.ends_with("）"),
            "{t}"
        );
        net_mut(&mut f).diag.second = crate::net_udp::SecondStun::NotAsked;
        assert_eq!(
            (mark_of(&f, N_LINK), mark_of(&f, N_STUN)),
            (mark_link, mark_stun)
        );
        net_mut(&mut f).connected = false;
        net_mut(&mut f).phase = 5;
        net_mut(&mut f).end = 1;
        net_mut(&mut f).end_detail = 1;
        assert!(
            text_of(&f, N_LINK).contains("診断: 自分の候補"),
            "{}",
            text_of(&f, N_LINK)
        );
        net_mut(&mut f).phase = 0;
        net_mut(&mut f).end = 0;
        f.stage = Stage::Interim(60);
        assert!(text_of(&f, N_LINK).starts_with("まだ繋がっていない"));
        assert!(
            text_of(&f, N_LINK).contains("診断: 自分の候補"),
            "{}",
            text_of(&f, N_LINK)
        );
        net_mut(&mut f).peer_cands = [0; 4];
        net_mut(&mut f).hello_sent = [0; 4];
        assert!(
            !text_of(&f, N_LINK).contains("診断"),
            "{}",
            text_of(&f, N_LINK)
        );
    }

    #[test]
    fn code_marks_and_mixups() {
        let mut f = good();
        assert_eq!(mark_of(&f, N_CODE), Mark::Ok);
        f.host.code.as_mut().unwrap().refused = 2;
        f.host.code.as_mut().unwrap().last_refusal = Some(Refusal::Broken);
        assert_eq!(
            mark_of(&f, N_CODE),
            Mark::Ok,
            "途中で失敗しても最後に読めた"
        );
        assert!(text_of(&f, N_CODE).contains("読めなかったことが 2 回"));
        let mut f = good();
        {
            let c = f.host.code.as_mut().unwrap();
            c.peer_read_at = None;
            c.connected_at = None;
            c.refused = 1;
            c.last_refusal = Some(Refusal::OwnCode);
        }
        assert_eq!(mark_of(&f, N_CODE), Mark::Bad);
        assert!(text_of(&f, N_CODE).contains("自分のコードを貼っている"));
        f.host.code.as_mut().unwrap().last_refusal = Some(Refusal::BothOffer);
        assert!(text_of(&f, N_CODE).contains("2 人とも始める側"));
        let mut f = good();
        {
            let c = f.host.code.as_mut().unwrap();
            c.peer_read_at = None;
            c.connected_at = None;
        }
        f.stage = Stage::Interim(30);
        f.net = Some(NetSnapshot::default());
        assert_eq!(mark_of(&f, N_CODE), Mark::Skip, "貼られるのを待っている間");
        f.host.code.as_mut().unwrap().gave_up = true;
        assert_eq!(mark_of(&f, N_CODE), Mark::Bad);
        assert!(text_of(&f, N_CODE).contains("起動し直す"));
        f.host.code.as_mut().unwrap().side = CodeSide::Game;
        let t = text_of(&f, N_CODE);
        assert!(
            t.contains("「妖怪 対 妖怪」のメニューからもう一度つなぐ") && !t.contains("起動し直す"),
            "{t}"
        );
        f.host.code = None;
        assert_eq!(mark_of(&f, N_CODE), Mark::Skip);
    }

    #[test]
    fn link_marks_and_failure_reasons() {
        let f = good();
        assert_eq!(mark_of(&f, N_LINK), Mark::Ok);
        let t = text_of(&f, N_LINK);
        assert!(
            t.contains("2.3 秒") && t.contains("家の中") && t.contains("作り直し 0 回"),
            "{t}"
        );
        let mut f = good();
        *net_mut(&mut f) = NetSnapshot {
            phase: 5,
            end: 1,
            end_detail: 1,
            hs_restarts: 3,
            recv_in: 0,
            ..NetSnapshot::default()
        };
        assert_eq!(mark_of(&f, N_LINK), Mark::Bad);
        let t = text_of(&f, N_LINK);
        assert!(
            t.contains("1 通も届かなかった")
                && t.contains("作り直し 3 回")
                && t.contains("受信ソケットに届いた 0 通"),
            "{t}"
        );
        net_mut(&mut f).recv_in = 4;
        net_mut(&mut f).recv_foreign = 4;
        net_mut(&mut f).last_foreign_ip = u32::from(std::net::Ipv4Addr::new(198, 51, 100, 9));
        net_mut(&mut f).last_foreign_port = 1 << 16 | 40_000;
        let t = text_of(&f, N_LINK);
        assert!(
            t.contains("候補に無い送り元 4 通、最後は 198.51.100.9:40000"),
            "{t}"
        );
        let mut f = good();
        f.stage = Stage::Interim(20);
        *net_mut(&mut f) = NetSnapshot::default();
        assert_eq!(mark_of(&f, N_LINK), Mark::Skip);
        f.stage = Stage::Final;
        assert_eq!(mark_of(&f, N_LINK), Mark::Bad);
        f.game.net = Setup::NotConfigured;
        assert_eq!(mark_of(&f, N_LINK), Mark::Skip);
    }

    #[test]
    fn select_marks() {
        let f = good();
        assert_eq!(mark_of(&f, N_SELECT), Mark::Ok);
        let t = text_of(&f, N_SELECT);
        assert!(
            t.contains("入った押下 6") && t.contains("繋がる前に落とした決定 1 回"),
            "{t}"
        );
        let mut f = good();
        net_mut(&mut f).agreed_mismatch = Some(0b011);
        assert_eq!(mark_of(&f, N_SELECT), Mark::Bad);
        assert!(text_of(&f, N_SELECT).contains("マップ・難易度"));
        net_mut(&mut f).agreed_mismatch = Some(0);
        net_mut(&mut f).menu_align_miss = 1;
        assert_eq!(mark_of(&f, N_SELECT), Mark::Bad);
        net_mut(&mut f).menu_align_miss = 0;
        net_mut(&mut f).menu_lost = 2;
        assert_eq!(mark_of(&f, N_SELECT), Mark::Bad);
        net_mut(&mut f).menu_lost = 0;
        net_mut(&mut f).end = 7;
        assert_eq!(mark_of(&f, N_SELECT), Mark::Bad);
        net_mut(&mut f).end = 6;
        assert_eq!(mark_of(&f, N_SELECT), Mark::Bad);
        let mut f = good();
        net_mut(&mut f).menu_applied = 0;
        assert_eq!(
            mark_of(&f, N_SELECT),
            Mark::Skip,
            "押下も揃った値も 0 件のまま 〇にしない"
        );
        net_mut(&mut f).agreed_mismatch = Some(0);
        assert_eq!(
            mark_of(&f, N_SELECT),
            Mark::Ok,
            "選択の画面を持たない偽のゲーム: 押下 0 で SELECT だけ揃う"
        );
        net_mut(&mut f).agreed_mismatch = None;
        net_mut(&mut f).connected = false;
        assert_eq!(mark_of(&f, N_SELECT), Mark::Skip);
        assert!(text_of(&f, N_SELECT).contains("繋がる前に落とした決定 1 回"));
    }

    #[test]
    fn match_marks() {
        let f = good();
        assert_eq!(mark_of(&f, N_MATCH), Mark::Ok);
        let t = text_of(&f, N_MATCH);
        assert!(
            t.contains("始まった試合 2")
                && t.contains("使ったフレーム 3600")
                && t.contains("stall 4（最長の連続 2）")
                && t.contains("CHECK の一致 12 回"),
            "{t}"
        );
        let mut f = good();
        net_mut(&mut f).charge_write_failures = 1;
        assert_eq!(mark_of(&f, N_MATCH), Mark::Bad);
        let mut f = good();
        net_mut(&mut f).segs_started = 0;
        assert_eq!(
            mark_of(&f, N_MATCH),
            Mark::Bad,
            "始まらないままネットワークが終わった"
        );
        net_mut(&mut f).phase = 4;
        assert_eq!(mark_of(&f, N_MATCH), Mark::Skip);
        net_mut(&mut f).connected = false;
        assert_eq!(mark_of(&f, N_MATCH), Mark::Skip);
    }

    #[test]
    fn sync_marks_and_desync_fields() {
        let f = good();
        assert_eq!(mark_of(&f, N_SYNC), Mark::Ok);
        assert!(text_of(&f, N_SYNC).contains("ズレ 0（CHECK が 12 回一致）"));
        let mut f = good();
        let (l, r) = (
            chk(1, 300, 0x1A2B),
            Check {
                rng_state: 0x3C4D,
                life_2p: 1,
                ..chk(1, 300, 0)
            },
        );
        net_mut(&mut f).desync = Some((l, r));
        net_mut(&mut f).end = 4;
        net_mut(&mut f).end_detail = 300;
        assert_eq!(mark_of(&f, N_SYNC), Mark::Bad);
        assert!(!text_of(&f, N_SYNC).contains("試合を止めた"));
        net_mut(&mut f).stop = 1;
        assert!(text_of(&f, N_SYNC).starts_with("ズレ（試合を止めた）。CHECK がずれた"));
        let mut peer = good();
        net_mut(&mut peer).stop = 1 | 4;
        net_mut(&mut peer).end = 2;
        net_mut(&mut peer).end_detail = 3;
        assert_eq!(mark_of(&peer, N_SYNC), Mark::Bad);
        assert!(text_of(&peer, N_SYNC).starts_with("ズレ（試合を止めた）。相手が"));
        let t = text_of(&f, N_SYNC);
        for want in [
            "seg 1 frame 300",
            "乱数の状態 0x1A2B",
            "乱数の状態 0x3C4D",
            "抽選回数 77",
            "1P ライフ 3",
            "2P ライフ 2",
            "2P ライフ 1",
            "食い違った欄: 乱数の状態・2P ライフ",
        ] {
            assert!(t.contains(want), "{want} が無い: {t}");
        }
        let mut f = good();
        net_mut(&mut f).check_matches = 0;
        assert_eq!(
            mark_of(&f, N_SYNC),
            Mark::Skip,
            "CHECK 0 回を「ズレ 0」にしない"
        );
        net_mut(&mut f).segs_started = 0;
        assert_eq!(mark_of(&f, N_SYNC), Mark::Skip);
    }

    #[test]
    fn rtt_marks() {
        let f = good();
        assert_eq!(mark_of(&f, N_RTT), Mark::Ok);
        assert!(!rtt_exceeds_delay(100, 3));
        assert!(rtt_exceeds_delay(101, 3));
        assert!(!rtt_exceeds_delay(500, 0));
        let mut f = good();
        net_mut(&mut f).rtt_ms = Some(180);
        assert_eq!(mark_of(&f, N_RTT), Mark::Bad);
        assert!(text_of(&f, N_RTT).contains("180 ms"));
        net_mut(&mut f).rtt_ms = None;
        assert_eq!(mark_of(&f, N_RTT), Mark::Skip);
        net_mut(&mut f).connected = false;
        assert_eq!(mark_of(&f, N_RTT), Mark::Skip);
    }

    #[test]
    fn rtt_in_rollback_never_says_waiting() {
        let mut f = good();
        net_mut(&mut f).rollback = true;
        net_mut(&mut f).delay = 1;
        net_mut(&mut f).rtt_ms = Some(20);
        assert_eq!(mark_of(&f, N_RTT), Mark::Ok);
        assert!(text_of(&f, N_RTT).contains("rollback・入力の遅れ D 1"));
        net_mut(&mut f).rtt_ms = Some(180);
        assert_eq!(
            mark_of(&f, N_RTT),
            Mark::Skip,
            "rollback は RTT だけで × にしない"
        );
        let t = text_of(&f, N_RTT);
        assert!(t.contains("180 ms") && t.contains("D 1"));
        assert!(t.contains("待たずに相手の入力を予測する"));
        assert!(t.contains("予測が外れたときの飛びが増えうる"));
        assert!(!t.contains("待つ見込み"), "{t}");
        net_mut(&mut f).rollback = false;
        assert_eq!(mark_of(&f, N_RTT), Mark::Bad);
        assert!(
            text_of(&f, N_RTT)
                .contains("毎フレーム相手の入力を待つ見込み。近い相手と試すか、遅延を増やす")
        );
    }

    #[test]
    fn ending_marks() {
        let mut f = good();
        assert_eq!(mark_of(&f, N_END), Mark::Ok);
        for (end, detail, mark) in [
            (0, 0, Mark::Skip),
            (1, 1, Mark::Bad),
            (2, 1, Mark::Ok),
            (2, 3, Mark::Bad),
            (2, 4, Mark::Bad),
            (3, 0, Mark::Bad),
            (4, 60, Mark::Bad),
            (5, 1, Mark::Ok),
            (5, 4, Mark::Bad),
            (6, 16, Mark::Bad),
            (7, 2, Mark::Bad),
            (8, 0, Mark::Skip),
            (9, 0, Mark::Skip),
            (10, 4, Mark::Skip),
            (11, 0, Mark::Bad),
            (12, 0, Mark::Bad),
        ] {
            net_mut(&mut f).end = end;
            net_mut(&mut f).end_detail = detail;
            assert_eq!(
                mark_of(&f, N_END),
                mark,
                "end {end} detail {detail}: {}",
                text_of(&f, N_END)
            );
        }
        net_mut(&mut f).end = 8;
        assert!(text_of(&f, N_END).contains("対戦が成立しないまま時間切れ"));
        use crate::netplay::End;
        use higanr_core::match_gate::NoMatch;
        for (why, want) in [
            (NoMatch::TimedOut, "時間切れ"),
            (NoMatch::PeerHasNoGate, "相手の HiganRTool が古いか"),
            (NoMatch::PeerLeft, "成立しないまま相手が閉じた"),
        ] {
            let (end, detail) = crate::net_watch::end_code(Some(End::Unmatched(why)));
            net_mut(&mut f).end = end;
            net_mut(&mut f).end_detail = detail;
            let t = text_of(&f, N_END);
            assert_eq!(mark_of(&f, N_END), Mark::Skip, "{why:?}: {t}");
            assert!(t.contains(want) && !t.contains("読めない"), "{why:?}: {t}");
        }
        f.net = None;
        assert_eq!(mark_of(&f, N_END), Mark::Skip);
    }

    #[test]
    fn another_version_is_named_with_both_numbers() {
        use crate::netplay::End;
        use higanr_core::handshake::FailReason;
        let want = format!(
            "相手のHiganRToolのバージョンが違います（自分 {PROTO_VER}・相手 0）。相手と同じzipのHiganRToolを使ってください。"
        );
        let mut f = good();
        let (end, detail) =
            crate::net_watch::end_code(Some(End::HandshakeFailed(FailReason::OtherVersion {
                theirs: 0,
            })));
        *net_mut(&mut f) = NetSnapshot {
            phase: 5,
            end,
            end_detail: detail,
            recv_in: 3,
            ..NetSnapshot::default()
        };
        for name in [N_LINK, N_END] {
            assert_eq!(mark_of(&f, name), Mark::Bad, "{name}");
            let t = text_of(&f, name);
            assert!(t.contains(&want), "{name}: {t}");
            assert!(
                !t.contains("届いて") && !t.contains("読めない"),
                "{name}: {t}"
            );
        }
        let mut f = good();
        {
            let c = f.host.code.as_mut().unwrap();
            c.peer_read_at = None;
            c.connected_at = None;
            c.refused = 1;
            c.last_refusal = Some(Refusal::OtherVersion { theirs: 0 });
        }
        assert_eq!(mark_of(&f, N_CODE), Mark::Bad);
        let t = text_of(&f, N_CODE);
        assert!(t.contains(&want) && !t.contains("文字が欠けて"), "{t}");
    }

    #[test]
    fn autosave_marks() {
        let f = good();
        assert_eq!(mark_of(&f, N_SAVE), Mark::Ok);
        assert!(text_of(&f, N_SAVE).contains("2 本保存した"));
        let mut f = good();
        f.host.replay.mark_failed = 1;
        f.host.replay.empty_file = 2;
        assert_eq!(mark_of(&f, N_SAVE), Mark::Bad);
        let t = text_of(&f, N_SAVE);
        assert!(
            t.contains("印を足せなかった 1 回")
                && t.contains("0 バイト） 2 回")
                && t.contains("3 回"),
            "{t}"
        );
        f.host.replay = ReplayFacts {
            panicked: true,
            ..ReplayFacts::default()
        };
        assert_eq!(mark_of(&f, N_SAVE), Mark::Bad);
        f.host.replay = ReplayFacts {
            attempts: 1,
            ..ReplayFacts::default()
        };
        assert_eq!(mark_of(&f, N_SAVE), Mark::Skip, "保存 0 本を 〇 にしない");
        f.game.replay = Setup::Failed("保存関数の先頭が署名と違う".to_string());
        assert_eq!(mark_of(&f, N_SAVE), Mark::Bad);
        assert!(text_of(&f, N_SAVE).contains("署名と違う"));
        f.game.replay = Setup::NotConfigured;
        assert_eq!(mark_of(&f, N_SAVE), Mark::Skip);
    }

    #[test]
    fn panic_marks_name_the_entrance() {
        let f = good();
        assert_eq!(mark_of(&f, N_PANIC), Mark::Ok);
        for (setup, word) in [
            (0, "対戦の通信の入口"),
            (1, "自動保存"),
            (2, "ペースの先頭（3 回）"),
        ] {
            let mut f = good();
            match setup {
                0 => f.host.net_panicked = true,
                1 => f.host.replay.panicked = true,
                _ => {
                    f.pace_end = Some(PaceEnd {
                        panics: 3,
                        ..PaceEnd::default()
                    })
                }
            }
            assert_eq!(mark_of(&f, N_PANIC), Mark::Bad, "{word}");
            assert!(
                text_of(&f, N_PANIC).contains(word),
                "{}",
                text_of(&f, N_PANIC)
            );
        }
        let mut f = good();
        f.game.input_hook = Wrote::NotTried;
        assert_eq!(mark_of(&f, N_PANIC), Mark::Skip);
    }

    #[test]
    fn a_game_menu_run_that_never_opened_the_net_says_so() {
        let mut f = good();
        f.host.open = None;
        f.host.code = None;
        f.net = Some(NetSnapshot::default());
        f.host.menu = MenuNet::NotAsked;
        for n in [
            N_OPEN, N_STUN, N_CODE, N_LINK, N_SELECT, N_MATCH, N_SYNC, N_RTT, N_END,
        ] {
            assert_eq!(mark_of(&f, n), Mark::Skip, "{n}");
            assert_eq!(
                text_of(&f, n),
                "ゲーム内のメニューで「作る」「貼る」を選ばなかったので開いていない",
                "{n}"
            );
        }
        f.host.menu = MenuNet::Asked;
        assert_eq!(mark_of(&f, N_OPEN), Mark::Bad);
        assert!(text_of(&f, N_OPEN).contains("開けていない"));
        f.stage = Stage::Interim(5);
        assert_eq!(mark_of(&f, N_OPEN), Mark::Skip);
        f.host.menu = MenuNet::NotMenu;
        f.stage = Stage::Final;
        assert_eq!(
            text_of(&f, N_OPEN),
            "受信ソケットを開いた（受信ソケットのアドレスは取れていない）"
        );
        let g = good();
        for menu in [MenuNet::NotMenu, MenuNet::NotAsked, MenuNet::Asked] {
            let mut g = g.clone();
            g.host.menu = menu;
            assert_eq!(mark_of(&g, N_OPEN), Mark::Ok, "{menu:?}");
            assert_eq!(mark_of(&g, N_LINK), Mark::Ok, "{menu:?}");
        }
    }

    #[test]
    fn a_run_without_the_net_marks_net_items_as_untried() {
        let mut f = good();
        f.game.net = Setup::NotConfigured;
        f.net = None;
        f.host = HostFacts::default();
        for n in [
            N_OPEN, N_STUN, N_CODE, N_LINK, N_SELECT, N_MATCH, N_SYNC, N_RTT, N_END,
        ] {
            assert_eq!(mark_of(&f, n), Mark::Skip, "{n}");
        }
        assert_eq!(mark_of(&f, N_VERSION), Mark::Ok);
    }

    #[test]
    fn the_code_item_names_the_viewer_side() {
        let mut f = good();
        f.host.code = Some(CodeFacts {
            peer_read_at: Some(2.5),
            ..CodeFacts::new(CodeSide::Spectate)
        });
        let t = text_of(&f, N_CODE);
        assert!(t.contains("見るだけの側"), "{t}");
        assert!(!t.contains("始める側") && !t.contains("返す側"), "{t}");
        assert_eq!(mark_of(&f, N_CODE), Mark::Ok);
    }

    /// 「繋がらなかった」回（家の中の相手・STUN は答えた・相手のコードは読めたが 10 秒 1 通も届かず 3 回作り直して終わった）の節の全文を見本として固定する（文言を変えたらここも直す）。
    #[test]
    fn a_link_that_never_came_up_reads_top_to_bottom() {
        let mut f = good();
        f.host.open.as_mut().unwrap().scope = Scope::Lan;
        f.host.code = Some(CodeFacts {
            peer_read_at: Some(41.8),
            ..CodeFacts::new(CodeSide::Answer)
        });
        f.host.replay = ReplayFacts::default();
        f.pace_end = Some(PaceEnd {
            rows: 5_400,
            ..PaceEnd::default()
        });
        *net_mut(&mut f) = NetSnapshot {
            phase: 5,
            end: 1,
            end_detail: 1,
            hs_restarts: 3,
            ..NetSnapshot::default()
        };
        let want = "\
==== 試験のまとめ ====
〇 花映塚のバージョン: 花映塚 1.50a と一致した
〇 フックの設置: 20 地点が全部一致し、入力のフックを書いた（描画のフックも書いた）
〇 フレームのペース: auto で刻んでいる。5400 フレームを記録、先頭でゲーム自身の経路へ任せた 0 回
〇 ネットワークを開けたか: 受信ソケットを開いた（受信ソケットのアドレスは家の中。scope = lan）
〇 STUN の答え: 外側のアドレスあり
〇 接続コードの受け渡し: 返す側。相手のコードを読めた（開いてから 41.8 秒）
× 繋がったか: 繋がらずに終わった: 10 秒のあいだ相手から 1 通も届かなかった。作り直し 3 回、受信ソケットに届いた 0 通（候補に無い送り元 0 通 / タグの誤り 0 通 / 別のセッション 0 通）。相手から 1 通も届いていない。相手のコードのアドレスが合っているか、相手が起動しているか、ファイアウォールとルーターを確かめる
－ ネットの選択: 繋がっていないので試していない（繋がる前に落とした決定 0 回）
－ 試合: 繋がっていないので試していない
－ 同期: 試合がまだ始まっていないので比べていない
－ RTT: 繋がっていない
× 終わりの理由: 接続できなかった（理由は「繋がったか」の行）
－ 自動保存: 試合の終わりをまだ見ていない（試みた 0 回）
〇 パニック: 受けていない
";
        assert_eq!(render(&f), want);
        eprintln!("{}", render(&f));
    }

    #[test]
    fn wording_has_no_internal_words() {
        let words = [
            "本人",
            "判例",
            "WIP",
            "§",
            "隣リポ",
            "【未】",
            "【推】",
            "【読】",
            "【測】",
            "【逆】",
            "逆アセンブル",
            "STATE.md",
        ];
        let mut texts = vec![render(&good())];
        let mut b = good();
        b.game = GameFacts::default();
        texts.push(render(&b));
        let mut c = good();
        c.game.net = Setup::Failed("x".into());
        c.game.replay = Setup::Failed("y".into());
        c.host.code.as_mut().unwrap().peer_read_at = None;
        c.host.code.as_mut().unwrap().last_refusal = Some(Refusal::BothAnswer);
        c.host.open.as_mut().unwrap().stun = Stun::NoAnswer("z".into());
        *net_mut(&mut c) = NetSnapshot {
            end: 4,
            desync: Some((chk(0, 60, 1), chk(0, 60, 2))),
            ..NetSnapshot::default()
        };
        texts.push(render(&c));
        for r in [
            Refusal::Unreadable,
            Refusal::OwnCode,
            Refusal::BothOffer,
            Refusal::BothAnswer,
            Refusal::Broken,
            Refusal::Expired,
            Refusal::WrongSession,
            Refusal::WrongKind,
            Refusal::NoRoute,
            Refusal::Internal,
            Refusal::OtherVersion { theirs: 0 },
        ] {
            texts.push(r.text());
        }
        for t in &texts {
            for w in words {
                assert!(
                    !t.to_lowercase().contains(&w.to_lowercase()),
                    "{w} が出た: {t}"
                );
            }
        }
    }

    #[test]
    fn marks_follow_the_item_order() {
        let mut f = good();
        assert_eq!(marks(&judge(&f)), vec![Mark::Ok; 14]);
        net_mut(&mut f).rtt_ms = None;
        let m = marks(&judge(&f));
        assert_eq!(m[10], Mark::Skip);
        assert_eq!(m.iter().filter(|x| **x != Mark::Ok).count(), 1);
    }
}
