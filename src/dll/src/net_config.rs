//! ネットワークの設定（`HiganRTool.ini` の `[netplay]` の節）を読む（pure。unsafe 無し）。
//!
//! ini は **DLL のフォルダ**（台本 `script.txt` と同じ所。ゲームのフォルダではない）に置く。`enable = 1` と書いた節があるときだけ
//! ネットワークを開く（台本が無くても開く。人が選択を押し、マッチを操作する）。知らないキーは断る（綴りの誤りで既定の値のまま走らないように）。
//! 宛先とソケットはネットワークの範囲 `scope` の中だけ（ガードは `higanr_core::guard`。開くときの `net_udp::open_scoped` でももう一度見る）。
//!
//! キー:
//! - `peer` は空白で区切って何個でも書ける（相手の候補の列。全部へ HELLO を送り、最初に返しが来た経路を採る）。
//! - `secret` = 接続の秘密（16 進 32 桁。peer_sim の `--secret` と揃える）。**無ければ、開くときに OS の乱数で引き**、
//!   自分の始める側の接続コード（秘密・session・自分のソケットを候補に）をログに書く。peer_sim は `--code` でそれを読む。
//! - `stun` = STUN の問い合わせ先（ネットワークの範囲の中だけ。空か無しなら問い合わせない）。`名前:ポート` も書ける（`StunTarget`。
//!   読むときは形だけ、引くのは開くときに読み取りのスレッドで）。
//! - `stun2` = 2 か所目の STUN の問い合わせ先（書き方は `stun` と同じ）。1 か所目と見えたアドレスを比べて、ルーターが宛先ごとに
//!   外側のポートを変える型かを診断に出す。1 か所目が答えないときだけ、2 か所目の答えを接続コードの候補にする。
//!   `scope = internet` で `stun` があるときだけ聞く（`net_udp::asks_second_stun`）。
//! - `ipv6` = `on` | `off`（既定 `on`）: IPv6 の経路。`on` で `scope = internet` のとき、OS の既定の送り元が世界に通じる IPv6
//!   （2000::/3）なら、そのアドレスで IPv6 のソケットを別に 1 本開き、自分のコードの候補に載せる（`net_udp::open_ipv6`）。`off` は
//!   IPv4 だけ。`scope` が internet でなければ使わない。外へ送るものは増えない（STUN は IPv4 のまま）。
//! - `upnp` = `on` | `off`（既定 `on`）: 家のルーターへ UPnP でソケットの UDP ポートの割り当てを頼む（`net_udp::upnp_begin`）。`scope = internet` で、
//!   ソケットが家の中の IPv4 のアドレスのときだけ頼む。割り当てた外側のポートは、ルーターの外側のアドレスが STUN で見えた
//!   アドレスと同じで世界に通じるときだけ自分の候補に載せ（違えば二重 NAT の見込みで割り当てを消す）、ネットワークを閉じたら消す（手で開けた期限なしの
//!   同じ割り当てには触れない）。UPnP がオフなら使えないまま進む。`off` は頼まない。
//! - `bind` = `IP:ポート` か `auto:ポート`（`Bind`。開くときに自分のアドレスを選ぶ）。
//! - `code_side` = `offer` | `answer`: 相手の候補を ini の `peer` で手で与える代わりに、接続コードを DLL のフォルダの `code\` で
//!   受け渡す（`net_code`）。`offer` は接続コードを**始める側**（秘密と session を作る）、`answer` は**返す側**。語はルームサーバーと
//!   同じ（`host | guest` はマッチのホストと紛れる）。`peer` と一緒には書けない（どちらの候補で繋ぐのかが読み手に分からない）。
//!   `answer` は `session` と `secret` も書けない（始める側のコードから取る）。`offer` は `session` を省けば開くときに OS の乱数で引く。
//! - `code_side = spectate`: **見るだけの側**。ホストが `spectate_code.txt` に書いた観戦用のコード（種別 3）を置き場の
//!   `peer_code.txt` から読み、ホストへ JOIN してマッチを観戦する（返しのコードは書かない。対戦用のコードは種別で断る）。
//!   相手のアドレス・session・秘密はコードから取るので `peer`・`session`・`secret` は書けない。STUN と自分のコードの候補
//!   （`stun`・`candidates`）も使わないので書けない。`code_dir`・`scope`・`bind`・`name`・`wait_budget_us` は使う。
//!   `[spectate]`（観戦のホスト）とは一緒に書けない。
//! - `code_side = game`: **ゲームの中のメニューで側を決める**。ソケット・STUN・見張りは起動のときには開かず（「妖怪 対 妖怪」が
//!   選ばれない限りゲームの動作に何も影響しない）、メニューの最初の「作る」「貼る」で別のスレッドが開き、見張りは側を決めずに
//!   待つ（`script_win::request_open` → `net_code_game::start_game`）。「接続コードを作る」で始める側、「相手の接続コードを貼る」で
//!   （自分のコードがまだ無ければ）返す側になる。コードはクリップボードで受け渡す。ローダーのウィンドウは開かない。`session`・`secret` は
//!   書けない（コードを作るたびに OS の乱数で引く）。
//! - `clipboard` = `system` | `file`（`code_side = game` と一緒にだけ。既定 `system`。開発用）: `file` は、本物のクリップボードの
//!   代わりに置き場（`code_dir`）の `clip_out.txt`（メニューがコピーする文字を書く）と `clip_in.txt`（メニューが貼る文字を読む）を使う
//!   （検査と実機の回が利用者のクリップボードを書き換えないため）。
//! - `candidates` = `all` | `mapped`（開発用。`code_side` と一緒にだけ。既定 `all`）: 自分の接続コードに載せる候補。`all` はソケットと、
//!   STUN で見えたアドレス。`mapped` は STUN で見えたアドレス**だけ**（同じ PC の 2 台をヘアピン NAT 越しに繋ぎ、外のアドレスだけで繋がるかを
//!   見る回のため）。`stun` が要り、STUN の答えが無ければネットワークを開かない（`net_code::start`）。配る ini には書かない。
//! - `code_dir` = 接続コードのファイルの置き場（`code_side` と一緒にだけ。相対なら DLL のフォルダから、無ければ DLL のフォルダの
//!   `code`）。同じ PC で花映塚を 2 つ起こすと DLL のフォルダは 1 つなので、ゲームごとに別の置き場にするため。
//! - `room_server` = ルームサーバーの宿。`https://名前[:ポート]`（`wss://` に読み替える。平文の `http://` は `127.0.0.1` /
//!   `localhost` だけ）。あればゲーム内のメニューに「ルームマッチ」が出る。空か無しなら出ない。`room` = ルーム名の**初めの値**
//!   （`room_server` と一緒にだけ。ルーム名はメニューの入力で打って決め、ここに書いた名はその入力の初めの値になる。メニューで決めた名は
//!   この行に書き戻される。無ければ空）。ルーム名は本ログに書かない。
//! - `head_wait` = `0` | `1`（既定 1）: delay 方式の lockstep で、フレームの始まり（自分の入力を読む前）に相手のワードを待つ
//!   （`netplay::NetPlay::head_wait`。上限 `netplay::HEAD_WAIT_CAP_US`）。0 は入力を読んだ後の待ちだけ。rollback・観戦者には
//!   効かない。ウィンドウモード（b-spin / b）は格子の後、全画面（ゲーム自身の時計）はゲームがその回にフレームを始める見込みのときだけ待つ
//!   （`pacing_win::pace_head`）。
//! - `rollback_hit_se` = `confirmed` | `live`（既定 `confirmed`）: rollback のとき、被弾音（id 4 とラストライフの 0x30）を
//!   確定するまで鳴らさない（`confirmed`。予測で起きて確定で起きなかった被弾の音が鳴らず、位置がずれても 1 回だけ鳴る。
//!   音だけが未確定の被弾のときに確定までの間遅れる）か、生きた tick で即座に鳴らす（`live`）か。delay 方式には予測が無いので
//!   効かない。入力 → 表示の遅れは変わらない（`rollback_resim::HitSe`）。
//! - `delay` = `auto` | `0`〜`8`（既定 auto）: メニューの「ディレイ」の初めの値。rollback の D と delay 方式の希望 latency の両方を
//!   これ 1 つから作る（`mode_pick::ini_delay`）。前の `latency` / `rollback_delay` は読まないが、書いてあっても節は断らず、本ログに 1 行出す。
//!
//! ネットワークの範囲（`higanr_core::guard` のモジュール doc）: `scope` = `loopback`（既定）| `lan` | `internet`。`bind`・`peer`・`stun` と、
//! 接続コードで受けた相手の候補の全部がこの範囲の中でなければ断る。知らない値（大文字・別の綴り）は既定に落とさず断り、
//! ネットワークを開かない（`lan` のつもりの誤記で loopback のまま開くと、繋がらない理由が設定だと分からないため）。`lan` / `internet` で
//! 相手へ届くには、`bind` にも自分のそのインターフェースのアドレスを書く（ソケットが 127.0.0.1 のままだと LAN の相手へ届かない。全部のインターフェース `0.0.0.0` は、
//! 自分の接続コードに載せられないのでどの範囲でも断る）。
//!
//! `after_script = keep` なら、台本が最後の段階まで押した後もネットワークを閉じない（「選択だけ台本 → 台本が終わって人が試合」の形。
//! 既定の `bye` は台本の終わりに BYE）。
//!
//! 置き場: ローダーの `--ini` / `--script` が子の環境の変数（`higanr_core::ini::PATH_ENV` / `higanr_core::script::PATH_ENV`）に
//! 置いたパスがあればそれを、無ければ DLL のフォルダの `HiganRTool.ini` / `script.txt` を読む（`locate`）。同じ PC で 2 つの
//! ゲームを起こすと DLL のフォルダは 1 つなので、ゲームごとに別の ini（ソケット）と台本を持たせるため。
#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use higanr_core::auth::Secret;
use higanr_core::candidate::{Candidate, Candidates, own_ipv6_candidate};
use higanr_core::code;
use higanr_core::delay_pick::DelayChoice;
use higanr_core::guard::{self, Scope};
use higanr_core::handshake::{self, ModeWish, Name};
use higanr_core::ini::{self, Ini};
use higanr_core::ini_lenient::IniNote;
use higanr_core::lan_addr::{self, LanAddr};
use higanr_core::link;
use higanr_core::mode_pick::{self, Pick, Shape};
use higanr_core::rollback;
use higanr_core::wire::{Mode, Seat};

use crate::net_room::{Base, BaseError};
use crate::netplay::{CheckSource, LatPlan, WAIT_BUDGET_US};
use crate::rollback_resim::HitSe;

pub const INI_FILE: &str = "HiganRTool.ini";
pub const SECTION: &str = "netplay";
/// ini の `delay = auto`（書いていないときも）の希望 latency（peer_sim の `--latency` の既定と同じ。両者の大きい方になる）。
pub const DEFAULT_LATENCY: u8 = 2;
/// `code_side = game` で `bind` が無い・読めないときのポート（配る ini の `bind = auto:17723` と同じ。LAN のホストが待つ既定のポートでもある）。
pub use higanr_core::lan_addr::DEFAULT_PORT as DEFAULT_BIND_PORT;
pub use higanr_core::player_name::DEFAULT_NAME;
/// 配る ini の `scope`・`stun`・`stun2`。`code_side = game` でこれらが読めないときに使う（`room_server` は `update::UPDATE_SERVER`、
/// `bind` は `DEFAULT_BIND_PORT`）。配る ini と食い違うと検査が落ちる。
const SHIPPED_SCOPE: Scope = Scope::Internet;
const SHIPPED_STUN: &str = "stun.cloudflare.com:3478";
const SHIPPED_STUN2: &str = "stun.l.google.com:19302";

/// ini の `name` から名乗る名前を作る。プレイヤー名の文字はルーム名と同じ ASCII の英数字・`-`・`_`・`@` だけ（`player_name::accepts_char`）
/// なので、それ以外の文字（日本語・空白・記号）は落とし、長さは `PLAYER_NAME_MAX` で切る。空になったら既定の名前。
/// 設定の誤りでネットワークを開かない形にはしない。
fn player_name_of(raw: Option<&str>) -> String {
    use higanr_core::player_name::{PLAYER_NAME_MAX, accepts_char};
    let kept: String = raw
        .unwrap_or("")
        .chars()
        .filter(|&c| accepts_char(c))
        .take(PLAYER_NAME_MAX)
        .collect();
    if kept.is_empty() {
        DEFAULT_NAME.to_string()
    } else {
        kept
    }
}

// 望むモードの既定（ini の `mode`）は core の `mode_pick::DEFAULT_MODE`（ウィンドウと共有）。実際に HELLO で名乗るかは `ANNOUNCE_ROLLBACK`（下）も見る。

/// rollback のモードを HELLO で名乗るか。false にすると、ini の `mode` が rollback でも delay 方式を載せる（true のまま
/// rollback に決まると、相手は予測で進めるのにこちらは待つ）。実際に名乗るのは入口を全部書けた実行だけ
/// （`NetSettings::mode_wish_if`）。false に戻せば全部 delay 方式（旗はこの 1 か所）。
pub const ANNOUNCE_ROLLBACK: bool = true;

const KEYS: [&str; 29] = [
    "room_server",
    "room",
    "lan_host",
    "enable",
    "scope",
    "bind",
    "peer",
    "code_side",
    "code_dir",
    "clipboard",
    "candidates",
    "session",
    "secret",
    "stun",
    "stun2",
    "ipv6",
    "upnp",
    "nonce",
    "delay",
    "host_seat",
    "name",
    "check",
    "wait_budget_us",
    "lat_plan",
    "after_script",
    "mode",
    "rollback_ahead",
    "rollback_hit_se",
    "head_wait",
];

/// `delay` の前の遅延のキー。読まないが、書いてあっても節を断らない（前に配った ini に `latency` が入っているので、断ると
/// ネットワークの設定が丸ごと使われなくなる）。
const OLD_DELAY_KEYS: [&str; 2] = ["latency", "rollback_delay"];

/// 観戦の節。`[netplay]` の `enable = 1` と一緒にだけ効く（ネットワークが無ければ観戦もしない）。この節が無い・`enable = 0` なら
/// 今までと 1 バイトも違わない。
pub const SPECTATE_SECTION: &str = "spectate";
const SPECTATE_KEYS: [&str; 2] = ["enable", "code_dir"];

/// ini の `[spectate] enable = 1` のときの設定（ホストの側。観戦者は `[netplay] code_side = spectate`）。
///
/// `code_side = game`（配る zip の形）では、観戦用のコードをファイルに書かず、ルームで会った相手のマッチのホストが観戦用のルーム
/// （`spectate_offer`）で観戦者へ渡す。この形では `[spectate]` は `enable` だけを読む（`code_dir` を書くと断る）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpectateSettings {
    /// 観戦用の接続コード `spectate_code.txt` の置き場（相対なら DLL のフォルダから。無ければ `[netplay]` の `code_dir`、それも
    /// 無ければ DLL のフォルダの `code`）。同じ PC で 3 台を起こすとき、ゲームごとに分けるため。`code_side = game` では使わない。
    pub code_dir: Option<String>,
}

fn spectate_of(ini: &Ini, notes: &mut Vec<IniNote>) -> Result<Option<SpectateSettings>, String> {
    if !ini.has_section(SPECTATE_SECTION) {
        return Ok(None);
    }
    for (k, line) in ini.keys(SPECTATE_SECTION) {
        if !SPECTATE_KEYS.contains(&k) {
            notes.push(IniNote::ignored(
                line,
                SPECTATE_SECTION,
                k,
                format!("知らないキー（読めるのは {}）", SPECTATE_KEYS.join(" / ")),
            ));
        }
    }
    let code_dir = match ini.get(SPECTATE_SECTION, "code_dir") {
        None => None,
        Some("") => return Err("[spectate] の code_dir が空".to_string()),
        Some(v) => Some(v.to_string()),
    };
    let enable = match ini.get(SPECTATE_SECTION, "enable") {
        Some("1") => true,
        Some("0") | None => false,
        Some(v) => {
            let why = format!("enable の値 {v:?} は 0 か 1");
            soft(notes, ini, SPECTATE_SECTION, "enable", Err(why), false)
        }
    };
    Ok(enable.then_some(SpectateSettings { code_dir }))
}

/// 読めた値を返す。読めなければ注を足して `default`（その行は無いものとして扱う）。
fn soft<T>(
    notes: &mut Vec<IniNote>,
    ini: &Ini,
    section: &str,
    key: &str,
    read: Result<T, String>,
    default: T,
) -> T {
    read.unwrap_or_else(|why| {
        let line = ini.line_of(section, key).unwrap_or(0);
        notes.push(IniNote::ignored(line, section, key, why));
        default
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AfterScript {
    /// 台本の終わりに BYE(終了) でネットワークを閉じる（既定）。
    #[default]
    Bye,
    /// ネットワークを閉じない（以後は人の入力で続く）。台本が途中で止まったとき（条件を待ちきれなかった）は BYE(中止) で閉じる
    /// （台本の想定から外れた画面で人に渡さない）。
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeSide {
    /// 始める側（秘密・session・自分の候補を載せたコードを書き、相手の返す側のコードを待つ）。
    Offer,
    /// 返す側（相手の始める側のコードを待ち、自分の候補とタグを載せたコードを返す）。
    Answer,
    /// 見るだけの側。ホストが書く観戦用のコード（種別 3）を置き場の `peer_code.txt` から読み、返しのコードは書かない。
    /// 対戦用のコード（種別 1・2）は種別で断る。対戦のセッション（`Link`）は作らず、ホストへ JOIN して流れを受けるだけ（`spectate_watch.rs`）。
    Spectate,
    /// ゲームの中のメニューで側を決める。ソケット・STUN・見張りは起動のときには開かず、メニューの最初の「作る」「貼る」で別の
    /// スレッドが開き、見張りは側を決めずに待つ。「接続コードを作る」で始める側、「相手の接続コードを貼る」は自分のコードが
    /// まだ無ければ返す側、あれば相手の返すコードとして読む。コードはクリップボード（ini の `clipboard`）で受け渡し、
    /// ファイルは使わない（開発用の `clipboard = file` を除く）。
    Game,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClipboardKind {
    #[default]
    System,
    /// 置き場（`code_dir`）の `clip_out.txt` / `clip_in.txt`（開発用。検査と実機の回が利用者のクリップボードを書き換えないため）。
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OwnCandidates {
    /// ソケットと、STUN で見えたアドレス（今までどおり）。
    #[default]
    All,
    /// STUN で見えたアドレスだけ（開発用。ヘアピン NAT を通る回）。
    Mapped,
}

impl OwnCandidates {
    pub fn text(self) -> &'static str {
        match self {
            OwnCandidates::All => "受信ソケットのアドレスと STUN で見えたアドレス",
            OwnCandidates::Mapped => "STUN で見えたアドレスだけ（candidates = mapped）",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub path: PathBuf,
    /// ローダーが環境の変数で渡したパス（`Some(変数の名前)`）か、DLL のフォルダ（`None`）か。
    pub from_env: Option<&'static str>,
}

impl Located {
    pub fn text(&self) -> String {
        let file = self
            .path
            .file_name()
            .map_or(String::new(), |n| n.to_string_lossy().into_owned());
        match self.from_env {
            Some(name) => format!("環境の変数 {name} で渡された {file}"),
            None => format!("DLL のフォルダの {file}"),
        }
    }
}

/// 置き場を決める（pure。環境の変数の値は呼ぶ側が読んで渡す）。`env_value` があればそれを使う。空・相対パスなら断る
/// （ローダーは絶対パスに直して渡すので、そうでないなら渡し方の誤り。黙って DLL のフォルダへ戻ると、2 つのゲームが同じ ini の
/// ソケットを取り合う）。無ければ DLL のフォルダの `file_name`。
pub fn locate(
    env_name: &'static str,
    env_value: Option<&OsStr>,
    dll_dir: Option<&Path>,
    file_name: &str,
) -> Result<Located, String> {
    match env_value {
        Some(v) => {
            let p = PathBuf::from(v);
            if v.is_empty() {
                return Err(format!("環境の変数 {env_name} が空"));
            }
            if !p.is_absolute() {
                return Err(format!(
                    "環境の変数 {env_name} の値 {} が絶対パスでない",
                    p.display()
                ));
            }
            Ok(Located {
                path: p,
                from_env: Some(env_name),
            })
        }
        None => {
            let dir = dll_dir.ok_or("DLL のフォルダが分からない")?;
            Ok(Located {
                path: dir.join(file_name),
                from_env: None,
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetSettings {
    /// ネットワークの範囲（ini の `scope`。既定 loopback）。ソケット・相手の候補・STUN の宛先と、送る直前のガードが使う。
    pub scope: Scope,
    /// 自分のソケット（手で書いたアドレスか `auto:ポート`。開くときに `net_udp::resolve_bind` がアドレスにする）。
    pub bind: Bind,
    /// 相手のソケット（候補の列の最初）。`code_side` の形では None（相手の候補は接続コードから）。
    pub peer: Option<SocketAddr>,
    /// 相手の候補の残り（`peer` の行の 2 つ目から）。
    pub extra_peers: Vec<SocketAddr>,
    pub code_side: Option<CodeSide>,
    /// 接続コードのファイルの置き場（ini の `code_dir`）。`code_side` の形だけ。
    pub code_dir: Option<String>,
    /// ゲームの中のメニューがコードを受け渡す場所（ini の `clipboard`）。`code_side = game` のときだけ書ける。
    pub clipboard: ClipboardKind,
    /// 自分の接続コードに載せる候補（ini の `candidates`。`code_side` の形だけ書ける。既定 `All`）。
    pub candidates: OwnCandidates,
    /// 両者で同じ値（接続コードの代わり）。`peer` の形では必ず Some。`code_side` の形では、始める側は ini の値か
    /// 開くときに OS の乱数で引く値（None）、返す側はいつも None（始める側のコードから取る）。
    pub session_id: Option<u32>,
    /// 接続の秘密。None なら開くときに OS の乱数で引く。
    pub secret: Option<Secret>,
    /// STUN の問い合わせ先（アドレスはネットワークの範囲の中だけ。名前は開くときに引いたアドレスをガードに通す。None なら問い合わせない）。
    pub stun: Option<StunTarget>,
    /// 2 か所目の STUN の問い合わせ先（ini の `stun2`。None なら聞かない。`stun` が無ければ書いてあっても聞かない）。
    pub stun2: Option<StunTarget>,
    /// IPv6 の経路を使うか（ini の `ipv6`。`on` が既定。`scope = internet` のときだけ実際に開く —— `net_udp::open_ipv6`）。
    pub ipv6: bool,
    /// UPnP でポートの割り当てを頼むか（ini の `upnp`。`on` が既定。`scope = internet` のときだけ実際に頼む —— `net_udp::upnp_begin`）。
    pub upnp: bool,
    /// 役割を決める乱数（大きい方がホスト）。無ければ開くときに時刻とプロセス番号から作る。
    pub role_nonce: Option<u64>,
    /// 希望 latency（ini の `delay`。0〜8。auto（書かないときも）なら `DEFAULT_LATENCY` で、`latency_auto`）。
    pub latency: u8,
    /// ini の `delay` が auto。delay 方式のとき、メニューを触らなければ auto で始める（`start_delay`）。
    pub latency_auto: bool,
    pub host_seat: Seat,
    /// ini に `host_seat` を書いたか。書いてあればメニューの「サイド」の初めの値はそれ（台本の回を決まった席で走らせる）。書いていなければ「おまかせ」。
    pub host_seat_given: bool,
    pub name: String,
    pub check: CheckSource,
    pub wait_budget_us: u32,
    /// latency の切替の予定（検査用。ホストのとき。`lat_plan = F:N F:N …` —— 使った lockstep のフレームの数が F に達したら
    /// delay を N に。空白で区切り、F の小さい順）。
    pub lat_plan: LatPlan,
    pub after_script: AfterScript,
    /// 観戦のホスト（`[spectate] enable = 1`）。None なら観戦しない。
    pub spectate: Option<SpectateSettings>,
    /// 望むモード（ini の `mode = rollback | delay`。既定 rollback）。両者が rollback を望んだときだけ rollback
    /// （`handshake::ModeWish`）。HELLO に載せるのは `mode_wish`（`ANNOUNCE_ROLLBACK` を見る）。
    pub mode: Mode,
    /// rollback に決まったときの D（ini の `delay`。0〜8。auto（書かないときも）なら `rollback::DEFAULT_DELAY` = 1 で、
    /// `rollback_delay_auto`）。ホストの値が使われる。
    pub rollback_delay: u8,
    /// ini の `delay` が auto（`latency_auto` と同じ値）。rollback のとき、メニューを触らなければ auto で始める（`start_delay`）。
    pub rollback_delay_auto: bool,
    /// rollback に決まったときの R（ini の `rollback_ahead`。1〜16、既定 `rollback::DEFAULT_AHEAD` = 8）。ネットワークに載せない。
    pub rollback_ahead: u8,
    /// rollback のときの被弾音の鳴らし方（ini の `rollback_hit_se = confirmed | live`。既定 `confirmed`。ネットワークに載せない）。
    pub rollback_hit_se: HitSe,
    /// ルームサーバーの宿（ini の `room_server`。`https://` は `wss://` に読み替える）。None ならゲーム内のメニューに「ルームマッチ」を
    /// 出さない。使うのは `code_side = game` だけ（ほかの形でも読み、使わない。配る ini の `code_side` の 1 行だけを書き換えた形が
    /// 読めるように）。
    pub room_server: Option<Base>,
    /// ルーム名の初めの値（ini の `room`。打った文字のまま）。メニューの「ルームマッチ」の入力の初めの値になるだけで、ルーム名そのものは
    /// 入力で決める。文字の決まり（`room::RoomName::parse`）は読むときに見ず、入力の初めの値にするとき（`room_input::initial`）に
    /// 見る（決まりの外なら黙って空へ落とす。ini で断るとネットワークごと開かない）。**本ログ・ini の行の文に名を書かない**
    /// （`RoomText` の `Debug` も名を出さない）。
    pub room: Option<RoomText>,
    /// LAN の「クライアント」「観戦」で最後に打ったホストのアドレス（ini の `lan_host`）。次にアドレスを打つときの初めの値になるだけで、
    /// 打つ前に接続しはしない。決まりの外（LAN のアドレスでない・形が違う）の値は読まずに注を残す。
    pub lan_host: Option<LanAddr>,
    /// delay 方式の lockstep で、フレームの始まり（自分の入力を読む前）に相手のワードを待つか（ini の `head_wait`。0 か 1、既定 1）。
    /// 待つ上限は `netplay::HEAD_WAIT_CAP_US`。rollback のモード・観戦者・ネットワークを使わない実行には効かない（読んでも使わない）。
    pub head_wait: bool,
    /// 書いてあったが読まなかった古いキー（`OLD_DELAY_KEYS`。書いた順）。読んだ側が本ログに 1 行ずつ出す（`old_key_notes`）。
    pub old_keys: Vec<&'static str>,
    /// 読めない行を捨てて既定値で動かした注（行の番号の順）。ログに全部、メニューに 1 か所。
    pub ini_notes: Vec<IniNote>,
}

/// ini の `room` の文字（ルーム名）。`{:?}` で設定を丸ごと本ログへ書いても名が出ないよう、`Debug` は中身を書かない。
#[derive(Clone, PartialEq, Eq)]
pub struct RoomText(pub String);

impl std::fmt::Debug for RoomText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RoomText(<ルーム名は記録しない>)")
    }
}

/// ini の `room_server` を読む。`https://` → `wss://`、`http://` → `ws://` に読み替えて `net_room::Base::parse` へ。平文は宿が
/// `127.0.0.1` か `localhost` のときだけ（接続コードは秘密を含むので、外へは TLS の上だけで渡す）。
fn room_server_of(v: &str) -> Result<Base, String> {
    let lower = v.get(..8).map(str::to_ascii_lowercase).unwrap_or_default();
    let ws = if lower.starts_with("https://") {
        format!("wss://{}", v.get(8..).unwrap_or(""))
    } else if lower.starts_with("http://") {
        format!("ws://{}", v.get(7..).unwrap_or(""))
    } else {
        v.to_string()
    };
    let b = Base::parse(&ws).map_err(|e| {
        format!(
            "room_server の値 {v:?} が読めない（{}）。形は https://名前[:ポート]",
            match e {
                BaseError::Scheme => "https:// で始まらない",
                BaseError::Host => "宿の名が空か、英数字・- ・. の外の文字がある",
                BaseError::Port => "ポートが 1〜65535 の数でない",
                BaseError::Path => "宿の後ろにパス・問い（/ ? #）が付いている",
            }
        )
    })?;
    if !b.secure
        && !matches!(
            b.host.to_ascii_lowercase().as_str(),
            "127.0.0.1" | "localhost"
        )
    {
        return Err(format!(
            "room_server の値 {v:?} は平文（http）。平文は 127.0.0.1 と localhost だけ（接続コードは秘密を含むので https で）"
        ));
    }
    Ok(b)
}

fn num(s: &str, key: &str) -> Result<u64, String> {
    let r = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16),
        None => s.parse::<u64>(),
    };
    r.map_err(|_| format!("{key} の値 {s:?} が数として読めない"))
}

fn num_in<T: TryFrom<u64>>(s: &str, key: &str) -> Result<T, String> {
    let v = num(s, key)?;
    T::try_from(v).map_err(|_| format!("{key} の値 {v} が大きすぎる"))
}

fn addr(s: &str, key: &str) -> Result<SocketAddr, String> {
    s.parse::<SocketAddr>()
        .map_err(|_| format!("{key} の値 {s:?} が IP:ポート として読めない（名前は受けない）"))
}

/// `bind = auto` が選んだアドレスの出どころ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrSource {
    /// 外へ出る経路の送り元（`net_udp::probe_route_ip`）。
    Route,
    /// 経路が引けなかったので、アダプターの一覧から選んだ（`adapter_pick::choose`）。
    Adapters,
}

/// 自分のソケット（ini の `bind`）。手で書いた `IP:ポート` か、開くときに自分のアドレスを選ぶ `auto:ポート`。
///
/// `auto` は、lan / internet で使う人が自分の LAN のアドレスを調べて ini に書く手間を消すため。アドレスは経路表を引かないと分からず
/// （OS の状態を見る = pure でない）、選ぶのは開くときに読み取りのスレッドで（`net_udp::resolve_bind`）。ここは形だけ持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bind {
    /// 手で書いたアドレス（ポート 0 なら OS が選ぶ）。
    Fixed(SocketAddr),
    /// `auto:ポート`（ポートは 1〜65535。0 は書けない —— 自分の接続コードに載せるアドレスはポートが決まっていないと相手へ渡せない）。
    Auto(u16),
}

impl Bind {
    /// 手で書いたアドレス。`auto` なら None。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn fixed(self) -> Option<SocketAddr> {
        match self {
            Bind::Fixed(a) => Some(a),
            Bind::Auto(_) => None,
        }
    }

    /// 開くときのソケットのアドレスを決める。`route` は「外へ出る出口の自分のアドレス」を引く道具（本物は `net_udp::probe_route_ip`。
    /// 検査はアドレスを注入する）。範囲 loopback では引かずに 127.0.0.1。lan / internet では IPv4 の出口のアドレスを採り、範囲のガード
    /// （`guard::check_bind`）を通らなければ、ネットワークを開かせないために理由と「手で書く」案内の文を返す。戻りの文字列はログの 1 行に足す。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn resolve(
        self,
        scope: Scope,
        route: impl FnOnce() -> Result<IpAddr, String>,
    ) -> Result<(SocketAddr, String), String> {
        self.resolve_by(scope, || route().map(|ip| (ip, AddrSource::Route)))
    }

    /// `resolve` の、アドレスの出どころ（経路かアダプターの一覧か）も返す道具を渡す形。出どころはログの文に出る。
    pub fn resolve_by(
        self,
        scope: Scope,
        pick: impl FnOnce() -> Result<(IpAddr, AddrSource), String>,
    ) -> Result<(SocketAddr, String), String> {
        let port = match self {
            Bind::Fixed(a) => return Ok((a, String::new())),
            Bind::Auto(p) => p,
        };
        let hint = "bind にアドレスを手で書いてください";
        if scope == Scope::Loopback {
            let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
            return Ok((
                a,
                format!("bind = auto:{port} は scope = loopback なので {a} に決めた"),
            ));
        }
        let (ip, source) = pick().map_err(|e| {
            format!("bind = auto:{port} で自分のアドレスを決められない（{e}）。{hint}")
        })?;
        let IpAddr::V4(v4) = ip else {
            return Err(format!(
                "bind = auto:{port} が選んだアドレス {ip} は IPv4 でない（auto は IPv4 だけ）。{hint}"
            ));
        };
        if v4.is_loopback() {
            return Err(format!(
                "bind = auto:{port} が選んだアドレス {v4} は loopback（外へ出る経路が無い）。{hint}"
            ));
        }
        let a = SocketAddr::new(IpAddr::V4(v4), port);
        guard::check_bind(a, scope, "bind = auto が選んだアドレス")
            .map_err(|e| format!("bind = auto:{port} を使えない: {e}。{hint}"))?;
        let what = match source {
            AddrSource::Route => "外へ出る経路の自分のアドレス",
            AddrSource::Adapters => {
                "外へ出る経路が引けないので、アダプターの一覧から選んだ LAN のアドレス"
            }
        };
        Ok((
            a,
            format!(
                "bind = auto:{port} から {a} に決めた（{what}。scope = {}）",
                scope.name()
            ),
        ))
    }

    pub fn text(self) -> String {
        match self {
            Bind::Fixed(a) => a.to_string(),
            Bind::Auto(p) => format!("auto:{p}（開くときに自分のアドレスを決める）"),
        }
    }
}

/// `bind` の値を読む。`IP:ポート` は範囲のガードを通す（`0.0.0.0` は断るまま）。`auto:ポート` はポートの範囲だけ。
fn bind_of(s: &str, scope: Scope) -> Result<Bind, String> {
    if s == "auto" || s.starts_with("auto:") {
        let p = s.strip_prefix("auto:").unwrap_or_default();
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "bind の値 {s:?}: auto の後にポートが要る（auto:47001 の形。1〜65535）"
            ));
        }
        let port = p
            .parse::<u16>()
            .ok()
            .filter(|&n| n != 0)
            .ok_or_else(|| format!("bind の値 {s:?}: auto のポート {p} は 1〜65535"))?;
        return Ok(Bind::Auto(port));
    }
    let a = addr(s, "bind")?;
    guard::check_bind(a, scope, "bind")?;
    Ok(Bind::Fixed(a))
}

/// STUN の問い合わせ先（ini の `stun`）。`IP:ポート` か `名前:ポート`（例 `stun.cloudflare.com:3478`）。
///
/// 公開の STUN のアドレスは事業者が変えうるので、利用者の ini にアドレスを書かせないよう名前を許す。名前を引くのは DNS への問い合わせ
/// （外への通信）なので、読むときは形だけ見て、引くのは開くときに読み取りのスレッドで（`net_udp::resolve_stun`。
/// ゲームのスレッドでは引かない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StunTarget {
    /// アドレス。読むときに範囲のガードを通してある。
    Addr(SocketAddr),
    /// 名前とポート。名前は `guard::check_host_name` を通してある。範囲のガードは引いた後のアドレスに掛ける。
    Name { host: String, port: u16 },
}

impl StunTarget {
    pub fn text(&self) -> String {
        match self {
            StunTarget::Addr(a) => a.to_string(),
            StunTarget::Name { host, port } => format!("{host}:{port}（開くときに名前を引く）"),
        }
    }
}

/// `stun` の値を読む。アドレスなら範囲のガードを通す。名前なら文字の形とポート（1〜65535）だけ（範囲は引いた後のアドレスで見る）。
fn stun_of(v: &str, scope: Scope) -> Result<StunTarget, String> {
    if let Ok(a) = v.parse::<SocketAddr>() {
        guard::check_peer(a, scope)?;
        return Ok(StunTarget::Addr(a));
    }
    let hint = "IP:ポート か 名前:ポート（例 stun.cloudflare.com:3478）で";
    let (host, p) = v
        .rsplit_once(':')
        .ok_or_else(|| format!("stun の値 {v:?} が読めない。{hint}"))?;
    let port = if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
        None
    } else {
        p.parse::<u16>().ok().filter(|&n| n != 0)
    }
    .ok_or_else(|| format!("stun の値 {v:?} のポート {p:?} は 1〜65535。{hint}"))?;
    guard::check_host_name(host).map_err(|e| format!("stun の値 {v:?}: {e}。{hint}"))?;
    Ok(StunTarget::Name {
        host: host.to_string(),
        port,
    })
}

/// ini の本文から読む。`[netplay]` の節が無い・`enable = 0` なら `Ok(None)`（ネットワークなしで走る）。
pub fn from_text(text: &str) -> Result<Option<NetSettings>, String> {
    let ini = ini::parse(text).map_err(|e| e.to_string())?;
    from_ini(&ini)
}

pub fn from_ini(ini: &Ini) -> Result<Option<NetSettings>, String> {
    if !ini.has_section(SECTION) {
        return Ok(None);
    }
    let mut old_keys = Vec::new();
    let mut notes = Vec::new();
    for (k, line) in ini.keys(SECTION) {
        if let Some(old) = OLD_DELAY_KEYS.iter().find(|o| **o == k) {
            old_keys.push(*old);
            continue;
        }
        if !KEYS.contains(&k) {
            notes.push(IniNote::ignored(
                line,
                SECTION,
                k,
                "知らないキー".to_string(),
            ));
        }
    }
    let get = |k: &str| ini.get(SECTION, k);
    match get("enable") {
        Some("1") => {}
        Some("0") | None => return Ok(None),
        Some(v) => return Err(format!("enable の値 {v:?} は 0 か 1")),
    }
    let need = |k: &str| get(k).ok_or(format!("[{SECTION}] に {k} が無い"));
    let code_side = match get("code_side") {
        None => None,
        Some("offer") => Some(CodeSide::Offer),
        Some("answer") => Some(CodeSide::Answer),
        Some("spectate") => Some(CodeSide::Spectate),
        Some("game") => Some(CodeSide::Game),
        Some(v) => {
            return Err(format!(
                "code_side の値 {v:?} は offer か answer か spectate か game"
            ));
        }
    };
    // ゲーム内のメニューの形（配る ini）は、読めない行を配る ini の値に戻す。ほかの形（開発用）は書かないときの値。
    let shipped = code_side == Some(CodeSide::Game);
    let scope = soft(
        &mut notes,
        ini,
        SECTION,
        "scope",
        match get("scope") {
            None => Ok(Scope::default()),
            Some(v) => {
                Scope::parse(v).ok_or_else(|| format!("scope の値 {v:?} は {}", Scope::NAMES))
            }
        },
        if shipped {
            SHIPPED_SCOPE
        } else {
            Scope::default()
        },
    );
    // ゲーム内のメニューの形は bind の既定を持つ。ほかの形（開発用）は従来どおり必須。
    let bind = match (code_side, get("bind")) {
        (Some(CodeSide::Game), None) => Bind::Auto(DEFAULT_BIND_PORT),
        (Some(CodeSide::Game), Some(v)) => soft(
            &mut notes,
            ini,
            SECTION,
            "bind",
            bind_of(v, scope),
            Bind::Auto(DEFAULT_BIND_PORT),
        ),
        _ => bind_of(need("bind")?, scope)?,
    };
    let spectate = spectate_of(ini, &mut notes)?;
    let clipboard = match (code_side, get("clipboard")) {
        (_, None) => ClipboardKind::default(),
        (Some(CodeSide::Game), Some("system")) => ClipboardKind::System,
        (Some(CodeSide::Game), Some("file")) => ClipboardKind::File,
        (Some(CodeSide::Game), Some(v)) => {
            return Err(format!("clipboard の値 {v:?} は system か file"));
        }
        (_, Some(_)) => {
            return Err(
                "clipboard は code_side = game と一緒にだけ書ける（ゲームの中のメニューが接続コードを受け渡す場所）"
                    .to_string(),
            );
        }
    };
    if code_side == Some(CodeSide::Game) {
        // コードを作るたびに OS の乱数で秘密と session を引く。ini で固定しても使われない値になるので断る。
        for k in ["session", "secret"] {
            if get(k).is_some() {
                return Err(format!(
                    "code_side = game では {k} を書けない（接続コードを作るたびに OS の乱数で引く。返す側のときは始める側のコードから取る）"
                ));
            }
        }
    }
    if code_side == Some(CodeSide::Spectate) {
        // 見るだけの側は、相手の候補・session・秘密を観戦用のコードから取り、対戦のセッションを作らない。書いても使われない値は断る。
        // delay・host_seat・check・lat_plan・after_script は対戦のセッションの値で、見るだけの側は使わない（ホストの ini をコピーして
        // code_side だけ直した形が読めるよう、断らずに読み捨てる）。
        for k in ["peer", "session", "secret", "stun", "stun2", "candidates"] {
            if get(k).is_some() {
                return Err(format!(
                    "code_side = spectate では {k} を書けない（相手のアドレス・session・秘密は観戦用のコードから取る。見るだけの側は STUN も自分のコードも使わない）"
                ));
            }
        }
        if spectate.is_some() {
            return Err(
                "code_side = spectate と [spectate]（観戦のホスト）は一緒に書けない（見るだけの側が観戦のホストにはなれない）"
                    .to_string(),
            );
        }
    }
    let (peer, extra_peers) = match code_side {
        Some(_) => {
            if get("peer").is_some() {
                return Err(
                    "peer と code_side は一緒に書けない（相手の候補を手で書くか、接続コードで受け渡すかのどちらか一方）"
                        .to_string(),
                );
            }
            (None, Vec::new())
        }
        None => {
            let mut peers = Vec::new();
            for p in need("peer")?.split_whitespace() {
                let a = addr(p, "peer")?;
                guard::check_peer(a, scope)?;
                peers.push(a);
            }
            // 候補のガード（数の上限・重なり）も読むときに見る（開いてから並べられないと分かるのを避ける）。
            Candidates::from_addrs(&peers)
                .map_err(|e| format!("peer の候補 {peers:?} が並べられない: {e:?}"))?;
            let (&peer, extra) = peers.split_first().ok_or("peer が空")?;
            (Some(peer), extra.to_vec())
        }
    };
    let code_dir = match (code_side, get("code_dir")) {
        (_, None) => None,
        (None, Some(_)) => {
            return Err(
                "code_dir は code_side と一緒にだけ書ける（接続コードのファイルの置き場）"
                    .to_string(),
            );
        }
        (Some(_), Some("")) => return Err("code_dir が空".to_string()),
        (Some(_), Some(v)) => Some(v.to_string()),
    };
    let candidates = match (code_side, get("candidates")) {
        (_, None) => OwnCandidates::default(),
        (None, Some(_)) => {
            return Err(
                "candidates は code_side と一緒にだけ書ける（自分の接続コードに載せる候補。peer の形には接続コードが無い）"
                    .to_string(),
            );
        }
        (Some(_), Some("all")) => OwnCandidates::All,
        (Some(_), Some("mapped")) => OwnCandidates::Mapped,
        (Some(_), Some(v)) => return Err(format!("candidates の値 {v:?} は all か mapped")),
    };
    if candidates == OwnCandidates::Mapped && get("stun").is_none_or(str::is_empty) {
        return Err(
            "candidates = mapped には stun が要る（STUN で見えたアドレスだけを候補にするので、問い合わせ先が無いと候補が空になる）"
                .to_string(),
        );
    }
    if code_side == Some(CodeSide::Answer) {
        for k in ["session", "secret"] {
            if get(k).is_some() {
                return Err(format!(
                    "code_side = answer では {k} を書けない（始める側の接続コードから取る）"
                ));
            }
        }
    }
    let session_id = match (code_side, get("session")) {
        (None, _) => Some(num_in::<u32>(need("session")?, "session")?),
        (Some(_), Some(v)) => Some(num_in::<u32>(v, "session")?),
        (Some(_), None) => None,
    };
    let secret = get("secret").map(Secret::from_hex).transpose()?;
    // `stun`・`stun2` が読めないときは、配る形なら配る ini の問い合わせ先、開発の形なら問い合わせない（書かないのと同じ）。
    let mut stun_of_key = |key: &str, shipped_value: &str| match get(key) {
        None | Some("") => None,
        Some(v) => soft(
            &mut notes,
            ini,
            SECTION,
            key,
            stun_of(v, scope).map(Some),
            shipped
                .then(|| stun_of(shipped_value, scope).ok())
                .flatten(),
        ),
    };
    let stun = stun_of_key("stun", SHIPPED_STUN);
    let stun2 = stun_of_key("stun2", SHIPPED_STUN2);
    // 空の値も書き損じとして注にする（`ipv6 =` を `off` と読むと、使うつもりが切れる）。
    let mut on_off = |key: &str| match get(key) {
        None | Some("on") => true,
        Some("off") => false,
        Some(v) => soft(
            &mut notes,
            ini,
            SECTION,
            key,
            Err(format!("{key} の値 {v:?} は on か off")),
            true,
        ),
    };
    let ipv6 = on_off("ipv6");
    let upnp = on_off("upnp");
    let role_nonce = get("nonce").map(|v| num(v, "nonce")).transpose()?;
    // rollback の D と delay 方式の希望 latency は同じ `delay` から作る。auto のときの数だけが方式ごとに違う
    // （D は `rollback::DEFAULT_DELAY`、latency は `DEFAULT_LATENCY`）。
    // 望むモードと D の読み方は、接続コードのウィンドウ（ローダー）が ini から今の形を出すのと同じでなければならないので
    // core の `mode_pick::ini_shape` に置く。読めない行は、その行だけ無いものとして渡す。
    let mut readable = |key: &'static str| {
        let alone = mode_pick::ini_shape(|k| if k == key { get(k) } else { None });
        match alone {
            Ok(_) => get(key),
            Err(why) => soft(&mut notes, ini, SECTION, key, Err(why), None),
        }
    };
    let (mode_v, delay_v) = (readable("mode"), readable(mode_pick::DELAY_KEY));
    let delay = mode_pick::ini_delay(delay_v).unwrap_or(DelayChoice::Auto);
    let latency = match delay {
        DelayChoice::Auto => DEFAULT_LATENCY,
        DelayChoice::Fixed(d) => d,
    };
    let delay_auto = delay == DelayChoice::Auto;
    let host_seat_wish = match get("host_seat") {
        None => None,
        Some("1") => Some(Seat::P1),
        Some("2") => Some(Seat::P2),
        Some(v) => soft(
            &mut notes,
            ini,
            SECTION,
            "host_seat",
            Err(format!("host_seat の値 {v:?} は 1 か 2")),
            None,
        ),
    };
    let host_seat_given = host_seat_wish.is_some();
    let host_seat = host_seat_wish.unwrap_or(Seat::P1);
    let name = player_name_of(get("name"));
    let check = match get("check") {
        None | Some("game") => CheckSource::Game,
        Some("digest") => CheckSource::InputDigest,
        Some(v) => soft(
            &mut notes,
            ini,
            SECTION,
            "check",
            Err(format!("check の値 {v:?} は game か digest")),
            CheckSource::Game,
        ),
    };
    let wait_budget_us = match get("wait_budget_us") {
        Some(v) => num_in::<u32>(v, "wait_budget_us")?,
        None => WAIT_BUDGET_US,
    };
    if wait_budget_us == 0 {
        return Err("wait_budget_us が 0（相手のワードを 1 度も待たない）".to_string());
    }
    let lat_plan = match get("lat_plan") {
        None => LatPlan::default(),
        Some(v) => lat_plan(v)?,
    };
    let after_script = match get("after_script") {
        None | Some("bye") => AfterScript::Bye,
        Some("keep") => AfterScript::Keep,
        Some(v) => return Err(format!("after_script の値 {v:?} は bye か keep")),
    };
    if code_side == Some(CodeSide::Game)
        && spectate.as_ref().is_some_and(|sp| sp.code_dir.is_some())
    {
        // ゲーム内のメニューの経路の観戦は、観戦用のコードをファイルに書かずルームサーバーで渡す（`spectate_offer`）。
        // 置き場は使われないので、書いたつもりで走らないよう断る。
        return Err(
            "code_side = game では [spectate] の code_dir を書けない（観戦用のコードはファイルに書かず、ルームサーバーで観戦者へ渡す）"
                .to_string(),
        );
    }
    // ルームサーバー。空の値は書かないのと同じ（`stun` と同じ扱い。配る ini の行を空にすればルームの項目が消える）。
    let room_server = match get("room_server") {
        None | Some("") => None,
        Some(v) => soft(
            &mut notes,
            ini,
            SECTION,
            "room_server",
            room_server_of(v).map(Some),
            shipped
                .then(|| room_server_of(higanr_core::update::UPDATE_SERVER).ok())
                .flatten(),
        ),
    };
    let room = match get("room") {
        None | Some("") => None,
        Some(_) if room_server.is_none() => soft(
            &mut notes,
            ini,
            SECTION,
            "room",
            Err("room は room_server と一緒にだけ書ける（ルームサーバーが無いとルーム名は使われない）"
                .to_string()),
            None,
        ),
        Some(v) => Some(RoomText(v.to_string())),
    };
    let lan_host = match get("lan_host") {
        None | Some("") => None,
        Some(v) => soft(
            &mut notes,
            ini,
            SECTION,
            "lan_host",
            lan_addr::parse(v).map(Some).map_err(|_| {
                format!(
                    "lan_host の値 {v:?} は 192.168.0.2 か 192.168.0.2:17723 の形（LAN のアドレスとポート）"
                )
            }),
            None,
        ),
    };
    let Shape {
        mode,
        delay: rollback_delay,
    } = mode_pick::ini_shape(|k| match k {
        "mode" => mode_v,
        mode_pick::DELAY_KEY => delay_v,
        _ => None,
    })?;
    let rollback_ahead = match get("rollback_ahead") {
        None => rollback::DEFAULT_AHEAD,
        Some(v) => num_in::<u8>(v, "rollback_ahead")?,
    };
    if !(rollback::MIN_AHEAD..=rollback::MAX_AHEAD).contains(&rollback_ahead) {
        return Err(format!(
            "rollback_ahead の値 {rollback_ahead} は {}〜{}",
            rollback::MIN_AHEAD,
            rollback::MAX_AHEAD
        ));
    }
    let rollback_hit_se = match get("rollback_hit_se") {
        None => HitSe::default(),
        Some(v) => soft(
            &mut notes,
            ini,
            SECTION,
            "rollback_hit_se",
            HitSe::parse(v),
            HitSe::default(),
        ),
    };
    let head_wait = match get("head_wait") {
        None | Some("1") => true,
        Some("0") => false,
        Some(v) => soft(
            &mut notes,
            ini,
            SECTION,
            "head_wait",
            Err(format!("head_wait の値 {v:?} は 0 か 1")),
            true,
        ),
    };
    Ok(Some(NetSettings {
        scope,
        bind,
        peer,
        extra_peers,
        code_side,
        code_dir,
        clipboard,
        candidates,
        session_id,
        secret,
        stun,
        stun2,
        ipv6,
        upnp,
        role_nonce,
        latency,
        latency_auto: delay_auto,
        host_seat,
        host_seat_given,
        name,
        check,
        wait_budget_us,
        lat_plan,
        after_script,
        spectate,
        mode,
        rollback_delay,
        rollback_delay_auto: delay_auto,
        rollback_ahead,
        rollback_hit_se,
        room_server,
        room,
        lan_host,
        head_wait,
        old_keys,
        ini_notes: notes,
    }))
}

fn lat_plan(v: &str) -> Result<LatPlan, String> {
    let mut items = Vec::new();
    for part in v.split_whitespace() {
        let Some((f, n)) = part.split_once(':') else {
            return Err(format!("lat_plan の {part:?} は F:N の形で"));
        };
        items.push((num(f, "lat_plan の F")?, num_in::<u8>(n, "lat_plan の N")?));
    }
    LatPlan::new(&items)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeFrom {
    /// ini の `mode` / `delay`。
    Ini,
    /// 接続コードのウィンドウで選んだ形（`mode.txt`）。
    Window,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unmet {
    /// 旗 `ANNOUNCE_ROLLBACK` が false。
    FlagOff,
    /// ini が `mode = delay`（rollback の入口を 1 バイトも書かない実行）。
    IniDelay,
    /// rollback のマッチの経路の入口を全部は書けていない。
    NotWired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeChoice {
    /// HELLO で名乗る形（`Link::set_mode` へ渡す）。
    pub wish: ModeWish,
    pub from: ModeFrom,
    /// ウィンドウで選んだ形（選んでいなければ None）。
    pub picked: Option<Pick>,
    /// ウィンドウの rollback の選択を採れなかった理由（採れたとき・rollback を選んでいないときは None）。
    pub unmet: Option<Unmet>,
}

impl ModeChoice {
    fn wish_text(&self) -> String {
        match self.wish.mode {
            Mode::Rollback => format!("rollback（D {}）", self.wish.rollback_delay),
            Mode::Delay => "delay 方式".to_string(),
        }
    }

    pub fn log_text(&self) -> String {
        let mut t = format!(
            "対戦の形: {}で名乗る: {}",
            match self.from {
                ModeFrom::Ini => "ini の形",
                ModeFrom::Window => "窓で選んだ形",
            },
            self.wish_text()
        );
        if let (Some(Pick::Rollback(d)), Some(u)) = (self.picked, self.unmet) {
            t.push_str(&format!(
                "（ウィンドウで選んだ rollback の D {d} は使えない: {}）",
                match u {
                    Unmet::FlagOff => "旗 ANNOUNCE_ROLLBACK が false",
                    Unmet::IniDelay =>
                        "ini が mode = delay で、rollback の入口を書いていない実行のため",
                    Unmet::NotWired =>
                        "rollback の試合の経路の入口を全部は書けていない（ログの巻き戻しの行を見る）",
                }
            ));
        }
        t
    }

    /// ウィンドウ（`status.txt`）の「状態」の行に足す文。ウィンドウで選んだ rollback を採れなかったときだけ。利用者が読む文で、内部の語を使わない。
    pub fn show_text(&self, lang: higanr_core::lang::Lang) -> Option<&'static str> {
        match (self.picked, self.unmet) {
            (Some(Pick::Rollback(_)), Some(Unmet::IniDelay)) => {
                Some(crate::show_text::ROLLBACK_INI_DELAY.get(lang))
            }
            (Some(Pick::Rollback(_)), Some(Unmet::FlagOff | Unmet::NotWired)) => {
                Some(crate::show_text::ROLLBACK_NOT_READY.get(lang))
            }
            _ => None,
        }
    }
}

impl NetSettings {
    pub fn peer_addrs(&self) -> Vec<SocketAddr> {
        let mut v: Vec<SocketAddr> = self.peer.into_iter().collect();
        v.extend_from_slice(&self.extra_peers);
        v
    }

    /// ini と旗（`ANNOUNCE_ROLLBACK`）から決まる、望むモード。ini が rollback で旗が true のときだけ rollback を望む。
    pub fn mode_wish(&self) -> ModeWish {
        match self.mode {
            Mode::Rollback if ANNOUNCE_ROLLBACK => ModeWish::rollback(self.rollback_delay),
            _ => ModeWish {
                mode: Mode::Delay,
                rollback_delay: self.rollback_delay,
            },
        }
    }

    /// HELLO で名乗る望むモード（`Link::set_mode` へ渡す）。`wired` は rollback のマッチの経路の入口を全部書けたか
    /// （`rollback_play_win::armed`）。書けていないのに rollback を名乗ると、相手は予測で進めるのにこちらは待つので、
    /// その実行は ini が rollback でも delay 方式を名乗る。
    pub fn mode_wish_if(&self, wired: bool) -> ModeWish {
        if wired {
            self.mode_wish()
        } else {
            ModeWish {
                mode: Mode::Delay,
                rollback_delay: self.rollback_delay,
            }
        }
    }

    /// ウィンドウで選んだ形 `pick`（接続コードのウィンドウが `mode.txt` に書いたもの。`None` なら選んでいない）を ini より先に採って、HELLO で
    /// 名乗る形を決める。ウィンドウの選択が ini に勝つが、**rollback を名乗れるのは今までと同じ条件のときだけ**（旗 `ANNOUNCE_ROLLBACK`・
    /// ini が rollback・入口を全部書けた `wired`）。ウィンドウで rollback を選んでも条件が揃わなければ delay 方式を名乗り、理由を
    /// `ModeChoice::unmet` で返す（ini が delay の実行は `arm_rollback` が 1 バイトも書かないので、ウィンドウの選択でも rollback のフックは増えない）。
    pub fn mode_choice(&self, wired: bool, pick: Option<Pick>) -> ModeChoice {
        let delay = ModeWish {
            mode: Mode::Delay,
            rollback_delay: self.rollback_delay,
        };
        let (wish, from, unmet) = match pick {
            None => (self.mode_wish_if(wired), ModeFrom::Ini, None),
            Some(Pick::Delay) => (delay, ModeFrom::Window, None),
            Some(Pick::Rollback(d)) => {
                let unmet = if !ANNOUNCE_ROLLBACK {
                    Some(Unmet::FlagOff)
                } else if self.mode == Mode::Delay {
                    Some(Unmet::IniDelay)
                } else if !wired {
                    Some(Unmet::NotWired)
                } else {
                    None
                };
                match unmet {
                    None => (ModeWish::rollback(d), ModeFrom::Window, None),
                    Some(u) => (delay, ModeFrom::Window, Some(u)),
                }
            }
        };
        ModeChoice {
            wish,
            from,
            picked: pick,
            unmet,
        }
    }

    pub fn handshake_config(&self, session_id: u32, role_nonce: u64) -> handshake::Config {
        handshake::Config {
            session_id,
            role_nonce,
            latency: self.latency,
            name: Name::new(&self.name),
            host_seat: self.host_seat,
        }
    }

    /// 繋がりのセッションの設定（`peer` の形。`secret` は ini の値か、開くときに OS の乱数で引いた値）。候補は読むときにガードを通してあるので、
    /// ここで並べられないことは無い（あれば候補を空にし、何も送らず何も受けない）。session も `peer` の形では読むときに必ず
    /// Some にしてある。`key_seed` は使い捨ての鍵の種（呼び手が OS の乱数で引く。`net_udp::draw_key_seed`）。
    pub fn link_config(
        &self,
        role_nonce: u64,
        secret: Secret,
        key_seed: higanr_core::seal::KeySeed,
    ) -> link::Config {
        link::Config {
            handshake: self.handshake_config(self.session_id.unwrap_or_default(), role_nonce),
            secret,
            peers: Candidates::from_addrs(&self.peer_addrs()).unwrap_or_default(),
            key_seed,
        }
    }

    /// 自分の候補の列（自分のソケット `local` と、STUN で見えたアドレス `mapped`、IPv6 のソケット `v6`。ソケットと同じなら重ねない）。
    /// 始める側のコードと返す側のコードの両方に載せる。`v6` は世界に通じるアドレス（`candidate::own_ipv6_candidate`）のときだけ載せる。
    pub fn own_candidates(
        local: SocketAddr,
        mapped: Option<SocketAddr>,
        v6: Option<SocketAddr>,
    ) -> Result<Candidates, String> {
        Self::own_candidates_upnp(local, mapped, v6, None)
    }

    /// `own_candidates` に、UPnP で割り当てた外側のポート `upnp_port` の候補を足したもの（STUN で見えた外側のアドレス ＋ そのポート）。
    pub fn own_candidates_upnp(
        local: SocketAddr,
        mapped: Option<SocketAddr>,
        v6: Option<SocketAddr>,
        upnp_port: Option<u16>,
    ) -> Result<Candidates, String> {
        let mut own = Candidates::EMPTY;
        for a in std::iter::once(local).chain(mapped) {
            if own.contains(a) {
                continue;
            }
            let c = Candidate::guess(a)
                .map_err(|e| format!("自分のアドレス {a} を候補にできない: {e:?}"))?;
            own.push(c)
                .map_err(|e| format!("自分の候補を並べられない: {e:?}"))?;
        }
        Self::push_local_port_on_mapped(&mut own, local, mapped)?;
        Self::push_upnp_port_on_mapped(&mut own, mapped, upnp_port)?;
        Self::push_own_v6(&mut own, v6)?;
        Ok(own)
    }

    /// STUN で見えた IPv4 のアドレスに、自分のソケットのポートを足す候補。ルーターが外側のポートを付け替えると、開けたポートは
    /// `mapped` のポートに無い。このアドレスとポートは開けたポートを指すので、相手の HELLO がそこへ届けば学んだ候補が本当の送り元へ返す。
    /// アドレスが IPv6 のときと、ポートが同じときは足さない。外側のアドレスと同じ優先で並ぶので、`mapped` の直後になる。
    fn push_local_port_on_mapped(
        own: &mut Candidates,
        local: SocketAddr,
        mapped: Option<SocketAddr>,
    ) -> Result<(), String> {
        let Some(m) = mapped.filter(|m| m.is_ipv4() && m.port() != local.port()) else {
            return Ok(());
        };
        let a = SocketAddr::new(m.ip(), local.port());
        if own.contains(a) {
            return Ok(());
        }
        let c = Candidate::guess(a).map_err(|e| {
            format!("STUN で見えたアドレスに受信ソケットのポートを足せない: {a}: {e:?}")
        })?;
        own.push(c)
            .map_err(|e| format!("自分の候補を並べられない: {e:?}"))?;
        Ok(())
    }

    /// STUN で見えた IPv4 の外側のアドレスに、UPnP で割り当てた外側のポートを足す候補。相手の HELLO がそこへ届けば、割り当てたポートを通って
    /// ソケットに着く。STUN の答えが無い（外側のアドレスが分からない）ときと、同じ候補が既にあるときは足さない。
    fn push_upnp_port_on_mapped(
        own: &mut Candidates,
        mapped: Option<SocketAddr>,
        upnp_port: Option<u16>,
    ) -> Result<(), String> {
        let (Some(m), Some(port)) = (mapped.filter(SocketAddr::is_ipv4), upnp_port) else {
            return Ok(());
        };
        let a = SocketAddr::new(m.ip(), port);
        if own.contains(a) {
            return Ok(());
        }
        let c = Candidate::guess(a)
            .map_err(|e| format!("UPnP の外側のポートの候補を作れない: {a}: {e:?}"))?;
        own.push(c)
            .map_err(|e| format!("自分の候補を並べられない: {e:?}"))?;
        Ok(())
    }

    fn push_own_v6(own: &mut Candidates, v6: Option<SocketAddr>) -> Result<(), String> {
        if let Some(c) = v6.and_then(own_ipv6_candidate)
            && !own.contains(c.addr)
        {
            own.push(c)
                .map_err(|e| format!("自分の IPv6 の候補を並べられない: {e:?}"))?;
        }
        Ok(())
    }

    /// 自分のコードに載せる候補（ini の `candidates`）。`All` は `own_candidates`。`Mapped` は STUN で見えたアドレス（`mapped`）だけで、
    /// ソケット `local` は載せない（IPv6 のソケット `v6` は世界に通じるアドレスなので載せる）。載せる候補が空になるとき
    /// （`mapped` も `v6` も無い）は Err（呼び手はネットワークを開かない。理由を本ログとウィンドウへ）。
    pub fn candidates_for(
        &self,
        local: SocketAddr,
        mapped: Option<SocketAddr>,
        v6: Option<SocketAddr>,
    ) -> Result<Candidates, String> {
        self.candidates_for_upnp(local, mapped, v6, None)
    }

    /// `candidates_for` に UPnP で割り当てた外側のポートの候補を足したもの。
    pub fn candidates_for_upnp(
        &self,
        local: SocketAddr,
        mapped: Option<SocketAddr>,
        v6: Option<SocketAddr>,
        upnp_port: Option<u16>,
    ) -> Result<Candidates, String> {
        match self.candidates {
            OwnCandidates::All => Self::own_candidates_upnp(local, mapped, v6, upnp_port),
            OwnCandidates::Mapped => {
                let mut own = Candidates::EMPTY;
                if let Some(m) = mapped {
                    let c = Candidate::guess(m).map_err(|e| {
                        format!("STUN で見えたアドレス {m} を候補にできない: {e:?}")
                    })?;
                    own.push(c)
                        .map_err(|e| format!("自分の候補を並べられない: {e:?}"))?;
                }
                Self::push_local_port_on_mapped(&mut own, local, mapped)?;
                Self::push_upnp_port_on_mapped(&mut own, mapped, upnp_port)?;
                Self::push_own_v6(&mut own, v6)?;
                if own.is_empty() {
                    return Err(
                        "candidates = mapped なのに STUN で見えたアドレスも世界に通じる IPv6 のアドレスも無い（載せる候補が空になるのでネットワークを開かない）"
                            .to_string(),
                    );
                }
                Ok(own)
            }
        }
    }

    /// 自分の始める側の接続コード。候補は `candidates_for`。有効期限は今から `code::DEFAULT_TTL_SECS`。
    pub fn initiator_code(
        &self,
        secret: Secret,
        session_id: u32,
        local: SocketAddr,
        mapped: Option<SocketAddr>,
        v6: Option<SocketAddr>,
        now_unix: u64,
    ) -> Result<String, String> {
        code::write_initiator(&code::Code {
            side: code::Side::Initiator,
            session_id,
            secret: Some(secret),
            name: Name::new(&self.name),
            nat: higanr_core::nat::NatMark::UNKNOWN,
            candidates: self.candidates_for(local, mapped, v6)?,
            expires: now_unix.saturating_add(code::DEFAULT_TTL_SECS),
        })
        .map_err(|e| format!("接続コードを書けない: {e:?}"))
    }

    /// ログ用の接続コードの置き場（ユーザー名を含みうる絶対パスは書かず、最後の 1 要素だけ）。
    fn code_dir_leaf(&self) -> &str {
        crate::log_mode::leaf(self.code_dir.as_deref().unwrap_or("code"))
    }

    pub fn text(&self) -> String {
        format!(
            "ネットワークの範囲 {} / 受信ソケット {} / 相手 {} / session {} / 秘密 {} / STUN {} / 2 か所目の STUN {} / nonce {} / latency {} / ホストの席 {} / 名前は記録しない / CHECK {} / 待ちの予算 {} µs / latency の切替の予定 {} / 台本の後 {} / 自分の候補 {} / モード {}",
            self.scope.name(),
            self.bind.text(),
            match self.code_side {
                None => self
                    .peer_addrs()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" "),
                Some(CodeSide::Offer) => format!(
                    "接続コードで受け渡す（始める側 offer。置き場 {} に my_code.txt を書き、peer_code.txt を待つ）",
                    self.code_dir_leaf()
                ),
                Some(CodeSide::Answer) => format!(
                    "接続コードで受け渡す（返す側 answer。置き場 {} の peer_code.txt を待ち、my_code.txt を書く）",
                    self.code_dir_leaf()
                ),
                Some(CodeSide::Spectate) => format!(
                    "観戦者（見るだけの側 spectate。置き場 {} の peer_code.txt に貼られた観戦用のコードを待ち、返しのコードは書かない）",
                    self.code_dir_leaf()
                ),
                Some(CodeSide::Game) => format!(
                    "接続コードで受け渡す（ゲームの中のメニューで側を決める game。コードの受け渡しは{}）",
                    match self.clipboard {
                        ClipboardKind::System => "クリップボード".to_string(),
                        ClipboardKind::File => format!(
                            "ファイル（開発用 clipboard = file。置き場 {} の {} / {}）",
                            self.code_dir_leaf(),
                            higanr_core::code_files::CLIP_OUT_FILE,
                            higanr_core::code_files::CLIP_IN_FILE
                        ),
                    }
                ),
            },
            match (self.session_id, self.code_side) {
                (Some(s), _) => format!("0x{s:08X}"),
                (None, Some(CodeSide::Answer)) => "始める側のコードから".to_string(),
                (None, Some(CodeSide::Spectate)) => "観戦用のコードから".to_string(),
                (None, Some(CodeSide::Game)) => {
                    "コードを作るときに OS の乱数で引く（返す側のときは始める側のコードから）"
                        .to_string()
                }
                (None, _) => "開くときに OS の乱数で引く".to_string(),
            },
            match (self.secret.is_some(), self.code_side) {
                (true, _) => "ini の値",
                (false, Some(CodeSide::Answer)) => "始める側のコードから",
                (false, Some(CodeSide::Spectate)) => "観戦用のコードから",
                (false, Some(CodeSide::Game)) => {
                    "コードを作るときに OS の乱数で引く（返す側のときは始める側のコードから）"
                }
                (false, _) => "開くときに OS の乱数で引く",
            },
            self.stun
                .as_ref()
                .map_or("問い合わせない".to_string(), StunTarget::text),
            self.stun2
                .as_ref()
                .map_or("聞かない".to_string(), StunTarget::text),
            self.role_nonce
                .map_or("（開くときに作る）".to_string(), |n| n.to_string()),
            self.latency_text(),
            match self.host_seat {
                Seat::P1 => "1P",
                Seat::P2 => "2P",
            },
            match self.check {
                CheckSource::Game => "ゲームの値",
                CheckSource::InputDigest => "入力の列のダイジェスト",
            },
            self.wait_budget_us,
            if self.lat_plan.is_empty() {
                "なし".to_string()
            } else {
                (0..self.lat_plan.len())
                    .filter_map(|i| self.lat_plan.get(i))
                    .map(|(f, n)| format!("{f}:{n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            },
            match self.after_script {
                AfterScript::Bye => "BYE で閉じる",
                AfterScript::Keep => "閉じない（人の入力で続く）",
            },
            self.candidates.text(),
            self.mode_text()
        ) + &self.room_text()
            + &format!(
                " / IPv6 の経路 {}",
                if self.ipv6 {
                    "使う（scope = internet のとき。世界に通じる IPv6 があれば）"
                } else {
                    "使わない（ini の ipv6 = off）"
                }
            )
            + &format!(
                " / UPnP {}",
                if self.upnp {
                    "頼む（scope = internet で、受信ソケットが家の中のアドレスのとき）"
                } else {
                    "頼まない（ini の upnp = off）"
                }
            )
    }

    /// ログの「ルームサーバー」の欄（宿と、ルーム名の初めの値の出どころ。**ルーム名そのものは書かない**）。
    fn room_text(&self) -> String {
        match &self.room_server {
            None => " / ルームサーバー 使わない".to_string(),
            Some(b) => format!(
                " / ルームサーバー {}://{}:{}（ルーム名はメニューで打つ。初めの値は {}。名は記録しない）{}",
                if b.secure { "wss" } else { "ws" },
                b.host,
                b.port,
                if self.room.is_some() {
                    "ini の room"
                } else {
                    "空"
                },
                if self.code_side == Some(CodeSide::Game) {
                    ""
                } else {
                    "（使うのは code_side = game だけ）"
                }
            ),
        }
    }

    fn mode_text(&self) -> String {
        let base = self.mode_text_ini();
        // 接続コードのウィンドウで形を選べる実行（offer / answer。観戦は選べない）では、この行は ini の形のまま。ウィンドウで選べば、後に出る
        // 接続コードの行と繋がった後の「対戦の形が決まった」の行が実際の形を言う。
        match self.code_side {
            Some(CodeSide::Offer | CodeSide::Answer) => {
                format!("{base}（接続コードのウィンドウで選べば、後の接続コードの行が勝つ）")
            }
            Some(CodeSide::Game) => {
                format!("{base}（ゲームの中のメニューで選べば、後の接続コードの行が勝つ）")
            }
            _ => base,
        }
    }

    /// 読み捨てた古いキーごとの本ログの 1 行。
    pub fn old_key_notes(&self) -> Vec<String> {
        self.old_keys
            .iter()
            .map(|k| format!("古いキー {k} は読まない。{} を使う", mode_pick::DELAY_KEY))
            .collect()
    }

    fn latency_text(&self) -> String {
        if self.latency_auto {
            format!("{}（auto）", self.latency)
        } else {
            self.latency.to_string()
        }
    }

    fn mode_text_ini(&self) -> String {
        let named = self.mode_wish().mode;
        match (self.mode, named) {
            (Mode::Delay, _) => "delay 方式".to_string(),
            (Mode::Rollback, Mode::Rollback) => format!(
                "rollback（D {}{}・R {}。相手も望めば rollback）",
                self.rollback_delay,
                if self.rollback_delay_auto {
                    "・auto"
                } else {
                    ""
                },
                self.rollback_ahead
            ),
            (Mode::Rollback, Mode::Delay) => format!(
                "ini は rollback（D {}・R {}）だが、旗 ANNOUNCE_ROLLBACK が false なので delay 方式を名乗る",
                self.rollback_delay, self.rollback_ahead
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str =
        "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 0x48475254\n";

    #[test]
    fn locate_prefers_the_env_and_refuses_bad_values() {
        let dll = Path::new(r"C:\tool\bin");
        let l = locate(
            higanr_core::ini::PATH_ENV,
            Some(OsStr::new(r"C:\s\two_host.ini")),
            Some(dll),
            INI_FILE,
        )
        .unwrap();
        assert_eq!(l.path, PathBuf::from(r"C:\s\two_host.ini"));
        assert_eq!(l.from_env, Some("HIGANRTOOL_INI"));
        // ログにはフォルダ（ユーザー名を含みうる）を書かず、ファイルの名前だけ。
        assert_eq!(
            l.text(),
            "環境の変数 HIGANRTOOL_INI で渡された two_host.ini"
        );
        let l = locate(higanr_core::ini::PATH_ENV, None, Some(dll), INI_FILE).unwrap();
        assert_eq!(l.path, PathBuf::from(r"C:\tool\bin\HiganRTool.ini"));
        assert_eq!(l.from_env, None);
        assert_eq!(l.text(), "DLL のフォルダの HiganRTool.ini");
        for bad in ["", r"two_host.ini", r"s\two_host.ini"] {
            let e = locate(
                higanr_core::script::PATH_ENV,
                Some(OsStr::new(bad)),
                Some(dll),
                "script.txt",
            )
            .unwrap_err();
            assert!(e.contains("HIGANRTOOL_SCRIPT"), "{bad:?}: {e}");
        }
        assert!(locate(higanr_core::ini::PATH_ENV, None, None, INI_FILE).is_err());
    }

    #[test]
    fn reads_the_minimal_section_with_defaults() {
        let s = from_text(OK).unwrap().unwrap();
        assert_eq!(s.bind, Bind::Fixed("127.0.0.1:0".parse().unwrap()));
        assert_eq!(s.peer, Some("127.0.0.1:7002".parse().unwrap()));
        assert_eq!(s.session_id, Some(0x4847_5254));
        assert_eq!(s.role_nonce, None);
        assert_eq!(
            (s.latency, s.host_seat, s.check, s.wait_budget_us),
            (2, Seat::P1, CheckSource::Game, 16_000)
        );
        assert_eq!(s.name, "HiganRTool");
        assert!(s.lat_plan.is_empty());
        assert_eq!(
            s.after_script,
            AfterScript::Bye,
            "台本の回の終わり方が変わる"
        );
        for k in ["bind", "peer", "session"] {
            let text: String = OK
                .lines()
                .filter(|l| !l.starts_with(k))
                .map(|l| format!("{l}\n"))
                .collect();
            let e = from_text(&text).unwrap_err();
            assert!(e.contains(k), "{k} が無いのに通した: {e}");
        }
    }

    #[test]
    fn reads_every_key() {
        let text = format!(
            "{OK}nonce = 99\ndelay = 4\nhost_seat = 2\nname = Test-Name_1\ncheck = digest\nwait_budget_us = 8000\nlat_plan = 600:4  1800:1 0x1000:9\nafter_script = keep\n"
        );
        let s = from_text(&text).unwrap().unwrap();
        assert_eq!(
            s.lat_plan,
            LatPlan::new(&[(600, 4), (1800, 1), (0x1000, 9)]).unwrap()
        );
        assert!(
            s.text()
                .contains("latency の切替の予定 600:4 1800:1 4096:9"),
            "{}",
            s.text()
        );
        assert_eq!(s.role_nonce, Some(99));
        assert_eq!(
            (s.latency, s.host_seat, s.check, s.wait_budget_us),
            (4, Seat::P2, CheckSource::InputDigest, 8000)
        );
        assert_eq!(s.name, "Test-Name_1");
        assert_eq!(s.after_script, AfterScript::Keep);
        assert!(s.text().contains("台本の後 閉じない"), "{}", s.text());
    }

    #[test]
    fn reads_the_mode_and_names_rollback_only_when_the_match_path_is_wired() {
        let s = from_text(OK).unwrap().unwrap();
        assert_eq!(
            (s.mode, s.rollback_delay, s.rollback_ahead),
            (Mode::Rollback, 1, 8),
            "既定"
        );
        // 旗 `ANNOUNCE_ROLLBACK` を false に戻すと下の 3 行で落ちる。
        const {
            assert!(
                ANNOUNCE_ROLLBACK,
                "旗が倒れていない（試合の経路は M8-2c で繋いだ）"
            )
        };
        assert_eq!(s.mode_wish().mode, Mode::Rollback);
        assert_eq!(
            s.mode_wish_if(true).mode,
            Mode::Rollback,
            "入口を書けた実行が名乗らない"
        );
        assert_eq!(
            s.mode_wish_if(false).mode,
            Mode::Delay,
            "入口を書けていない実行が rollback を名乗る"
        );
        assert!(
            s.text()
                .contains("rollback（D 1・auto・R 8。相手も望めば rollback）"),
            "{}",
            s.text()
        );
        let s = from_text(&format!(
            "{OK}mode = delay\ndelay = 0\nrollback_ahead = 16\n"
        ))
        .unwrap()
        .unwrap();
        assert_eq!(
            (s.mode, s.rollback_delay, s.rollback_ahead),
            (Mode::Delay, 0, 16)
        );
        assert_eq!(s.mode_wish().mode, Mode::Delay);
        assert_eq!(s.mode_wish_if(true).mode, Mode::Delay);
        assert!(s.text().contains("モード delay 方式"), "{}", s.text());
        let s = from_text(&format!(
            "{OK}mode = rollback\ndelay = 8\nrollback_ahead = 1\n"
        ))
        .unwrap()
        .unwrap();
        assert_eq!(
            (s.mode, s.rollback_delay, s.rollback_ahead),
            (Mode::Rollback, 8, 1)
        );
        assert_eq!(s.mode_wish().rollback_delay, 8, "D は旗に依らず渡す");
        assert_eq!(
            s.mode_wish_if(false).rollback_delay,
            8,
            "D は入口の有無に依らず渡す"
        );
        for bad in ["rollback_ahead = 0", "rollback_ahead = 17"] {
            let e = from_text(&format!("{OK}{bad}\n")).unwrap_err();
            assert!(e.contains("rollback_ahead"), "{bad}: {e}");
        }
        // mode・delay の読めない行は、その行だけ既定にして注を残す。
        for (bad, key) in [
            ("mode = Rollback", "mode"),
            ("mode = 1", "mode"),
            ("delay = 9", "delay"),
        ] {
            let s = from_text(&format!("{OK}{bad}\n")).unwrap().unwrap();
            assert_eq!(
                (s.mode, s.rollback_delay, s.rollback_delay_auto),
                (Mode::Rollback, 1, true),
                "{bad}"
            );
            assert_eq!(note_keys(&s), vec![key], "{bad}");
        }
        let s = from_text(&format!("{OK}mode = delay\ndelay = 9\n"))
            .unwrap()
            .unwrap();
        assert_eq!((s.mode, s.latency_auto), (Mode::Delay, true), "mode は採る");
        assert_eq!(note_keys(&s), vec!["delay"]);
    }

    #[test]
    fn the_window_reads_the_ini_shape_like_the_dll() {
        for extra in [
            "",
            "mode = delay\n",
            "mode = rollback\ndelay = 0\n",
            "mode = rollback\ndelay = 8\n",
            "mode = rollback\ndelay = 9\n",
            "delay = 0x2\n",
            "delay = auto\n",
            "mode = delay\ndelay = 7\n",
            "mode = rollback\nrollback_delay = 5\n",
            "mode = rollback\nrollback_delay = 5\ndelay = 2\n",
            "mode = Rollback\n",
            "mode = 1\n",
            "delay = 10\n",
            "delay = x\n",
        ] {
            let text = format!("{OK}{extra}");
            let dll = from_text(&text).map(|o| o.map(|s| (s.mode, s.rollback_delay)));
            let window = mode_pick::ini_shape_of_text(&text, SECTION)
                .expect("ini として読める")
                .map(|s| (s.mode, s.delay));
            match (dll, window) {
                (Ok(Some(a)), Ok(b)) => assert_eq!(a, b, "{extra:?}"),
                // ウィンドウは読めない行で断る。dll はその行だけ捨てて続ける。
                (Ok(Some(_)), Err(_)) => {
                    let s = from_text(&text).unwrap().unwrap();
                    assert!(!s.ini_notes.is_empty(), "{extra:?}");
                }
                (d, w) => panic!("{extra:?}: dll {d:?} とウィンドウ {w:?} が食い違う"),
            }
        }
    }

    #[test]
    fn a_window_pick_beats_the_ini_but_rollback_is_named_only_when_it_can_be() {
        let ini = |extra: &str| from_text(&format!("{OK}{extra}")).unwrap().unwrap();
        let rb5 = ini("mode = rollback\ndelay = 5\n");
        let dl3 = ini("mode = delay\ndelay = 3\n");
        let named = |c: ModeChoice| (c.wish.mode, c.wish.rollback_delay, c.from, c.unmet);
        assert_eq!(
            named(rb5.mode_choice(true, None)),
            (Mode::Rollback, 5, ModeFrom::Ini, None)
        );
        assert_eq!(
            named(rb5.mode_choice(false, None)),
            (Mode::Delay, 5, ModeFrom::Ini, None)
        );
        assert_eq!(rb5.mode_choice(true, None).wish, rb5.mode_wish_if(true));
        assert_eq!(rb5.mode_choice(false, None).wish, rb5.mode_wish_if(false));
        assert_eq!(dl3.mode_choice(true, None).wish, dl3.mode_wish_if(true));
        for d in 0..=2u8 {
            assert_eq!(
                named(rb5.mode_choice(true, Some(Pick::Rollback(d)))),
                (Mode::Rollback, d, ModeFrom::Window, None),
                "D {d}"
            );
        }
        assert_eq!(
            named(rb5.mode_choice(true, Some(Pick::Delay))),
            (Mode::Delay, 5, ModeFrom::Window, None)
        );
        assert_eq!(
            named(dl3.mode_choice(false, Some(Pick::Rollback(1)))),
            (Mode::Delay, 3, ModeFrom::Window, Some(Unmet::IniDelay))
        );
        assert_eq!(
            named(dl3.mode_choice(true, Some(Pick::Rollback(1)))),
            (Mode::Delay, 3, ModeFrom::Window, Some(Unmet::IniDelay)),
            "入口があっても ini が delay なら名乗らない"
        );
        assert_eq!(
            named(rb5.mode_choice(false, Some(Pick::Rollback(1)))),
            (Mode::Delay, 5, ModeFrom::Window, Some(Unmet::NotWired))
        );
        assert_eq!(
            named(dl3.mode_choice(true, Some(Pick::Delay))),
            (Mode::Delay, 3, ModeFrom::Window, None)
        );
        let said = |c: ModeChoice| c.show_text(higanr_core::lang::Lang::Ja);
        assert!(
            said(dl3.mode_choice(false, Some(Pick::Rollback(1))))
                .is_some_and(|t| t.contains("設定ファイル"))
        );
        assert!(
            said(rb5.mode_choice(false, Some(Pick::Rollback(1))))
                .is_some_and(|t| t.contains("準備ができなかった"))
        );
        for c in [
            rb5.mode_choice(true, Some(Pick::Rollback(0))),
            rb5.mode_choice(true, Some(Pick::Delay)),
            rb5.mode_choice(true, None),
            dl3.mode_choice(false, Some(Pick::Delay)),
            rb5.mode_choice(false, None),
        ] {
            assert_eq!(said(c), None, "{c:?}");
        }
        let t = rb5.mode_choice(true, Some(Pick::Rollback(0))).log_text();
        assert!(t.contains("窓で選んだ形で名乗る: rollback（D 0）"), "{t}");
        let t = rb5.mode_choice(true, None).log_text();
        assert!(t.contains("ini の形で名乗る: rollback（D 5）"), "{t}");
        let t = dl3.mode_choice(true, Some(Pick::Rollback(2))).log_text();
        assert!(
            t.contains("delay 方式") && t.contains("D 2 は使えない") && t.contains("mode = delay"),
            "{t}"
        );
    }

    #[test]
    fn reads_secret_candidates_and_stun() {
        let s = from_text(OK).unwrap().unwrap();
        assert_eq!((s.secret, s.stun.clone()), (None, None));
        assert!(s.text().contains("OS の乱数で引く"), "{}", s.text());
        let text = OK.replace(
            "peer = 127.0.0.1:7002",
            "peer = 127.0.0.1:7002 [::1]:7003  127.0.0.5:7004",
        ) + "secret = 000102030405060708090A0B0C0D0E0F\nstun = 127.0.0.1:3478\n";
        let s = from_text(&text).unwrap().unwrap();
        assert_eq!(s.peer, Some("127.0.0.1:7002".parse().unwrap()));
        assert_eq!(
            s.extra_peers,
            vec![
                "[::1]:7003".parse::<SocketAddr>().unwrap(),
                "127.0.0.5:7004".parse().unwrap()
            ]
        );
        assert_eq!(
            s.secret.map(|x| x.to_hex()),
            Some("000102030405060708090a0b0c0d0e0f".to_string())
        );
        assert_eq!(
            s.stun,
            Some(StunTarget::Addr("127.0.0.1:3478".parse().unwrap()))
        );
        assert!(
            !s.text().contains("0a0b0c"),
            "ログに秘密を出した: {}",
            s.text()
        );
        let lc = s.link_config(7, s.secret.unwrap(), higanr_core::seal::KeySeed([7; 32]));
        assert_eq!(lc.peers.len(), 3);
        assert_eq!(
            (lc.handshake.session_id, lc.handshake.role_nonce),
            (0x4847_5254, 7)
        );
        assert_eq!(
            from_text(&format!("{OK}stun =\n")).unwrap().unwrap().stun,
            None
        );
        for bad in [
            format!("{OK}secret = 0001\n"),
            format!("{OK}secret = zz0102030405060708090a0b0c0d0e0f\n"),
            OK.replace(
                "peer = 127.0.0.1:7002",
                "peer = 127.0.0.1:7002 192.168.1.2:7002",
            ),
            OK.replace(
                "peer = 127.0.0.1:7002",
                "peer = 127.0.0.1:7002 127.0.0.1:7002",
            ),
            OK.replace("peer = 127.0.0.1:7002", "peer = "),
            OK.replace(
                "peer = 127.0.0.1:7002",
                &format!(
                    "peer = {}",
                    (1..=9)
                        .map(|p| format!("127.0.0.1:{p}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            ),
        ] {
            assert!(from_text(&bad).is_err(), "通した: {bad}");
        }
        // 範囲の外の STUN は問い合わせない（その行だけ捨てる）。
        let s = from_text(&format!("{OK}stun = 10.0.0.1:3478\n"))
            .unwrap()
            .unwrap();
        assert_eq!((&s.stun, note_keys(&s)), (&None, vec!["stun"]));
    }

    #[test]
    fn initiator_code_carries_secret_and_own_candidates() {
        let s = from_text(OK).unwrap().unwrap();
        let secret = Secret([0x5A; 16]);
        let local: SocketAddr = "127.0.0.1:47001".parse().unwrap();
        let mapped: SocketAddr = "127.0.0.2:47001".parse().unwrap();
        let t = s
            .initiator_code(secret, 0x4847_5254, local, Some(mapped), None, 1_000)
            .unwrap();
        let c = code::read_initiator(&t, 1_000 + code::DEFAULT_TTL_SECS).unwrap();
        assert_eq!((c.session_id, c.secret), (0x4847_5254, Some(secret)));
        assert_eq!(c.name.as_str(), "HiganRTool");
        let addrs: Vec<SocketAddr> = c.candidates.as_slice().iter().map(|x| x.addr).collect();
        assert_eq!(addrs, vec![local, mapped]);
        assert!(code::read_initiator(&t, 1_001 + code::DEFAULT_TTL_SECS).is_err());
        let t = s
            .initiator_code(secret, 0x4847_5254, local, Some(local), None, 1_000)
            .unwrap();
        assert_eq!(code::read_initiator(&t, 0).unwrap().candidates.len(), 1);
    }

    #[test]
    fn off_unless_enabled() {
        assert_eq!(from_text("[other]\nk = 1\n"), Ok(None));
        assert_eq!(from_text(&OK.replace("enable = 1\n", "")), Ok(None));
        assert_eq!(from_text(&OK.replace("enable = 1", "enable = 0")), Ok(None));
        assert!(from_text(&OK.replace("enable = 1", "enable = yes")).is_err());
    }

    /// 読めない行があって、その行を捨てた（注が付いた）か、全体を断った。
    fn refused(text: &str) -> bool {
        match from_text(text) {
            Ok(Some(s)) => !s.ini_notes.is_empty(),
            Ok(None) => false,
            Err(_) => true,
        }
    }

    fn note_keys(s: &NetSettings) -> Vec<&str> {
        s.ini_notes.iter().map(|n| n.key.as_str()).collect()
    }

    /// （rollback の D・その auto、delay 方式の希望 latency・その auto）。
    fn delays(s: &NetSettings) -> (u8, bool, u8, bool) {
        (
            s.rollback_delay,
            s.rollback_delay_auto,
            s.latency,
            s.latency_auto,
        )
    }

    #[test]
    fn one_delay_key_feeds_both_modes_and_unwritten_is_auto() {
        let read = |extra: &str| from_text(&format!("{OK}{extra}")).unwrap().unwrap();
        // auto の数は方式ごとの既定（D 1・latency 2）。
        assert_eq!(delays(&read("")), (1, true, 2, true), "書かなければ auto");
        assert_eq!(delays(&read("delay = auto\n")), (1, true, 2, true));
        assert_eq!(delays(&read("delay = AUTO\n")), (1, true, 2, true));
        for n in [0u8, 3, 8] {
            for mode in ["rollback", "delay"] {
                let s = read(&format!("mode = {mode}\ndelay = {n}\n"));
                assert_eq!(
                    delays(&s),
                    (n, false, n, false),
                    "mode = {mode}・delay = {n}"
                );
                assert_eq!(s.mode_wish().rollback_delay, n);
                assert_eq!(s.handshake_config(1, 1).latency, n);
            }
        }
        for bad in ["9", "10", "-1", "autoo", "x", "", "1.5"] {
            let s = read(&format!("delay = {bad}\n"));
            assert_eq!(delays(&s), (1, true, 2, true), "{bad:?}");
            assert_eq!(note_keys(&s), vec!["delay"], "{bad:?}");
        }
        assert!(read("").old_keys.is_empty());
        assert!(read("").old_key_notes().is_empty());
    }

    #[test]
    fn the_old_delay_keys_are_read_past_without_refusing_the_section() {
        let read = |extra: &str| from_text(&format!("{OK}{extra}")).unwrap().unwrap();
        // 前に配った ini の形（`latency = 2`）と、古いキーの 2 つ。値は読まない（範囲の外や文字でも断らない）。
        for (extra, want) in [
            ("latency = 2\n", vec!["latency"]),
            ("rollback_delay = 5\n", vec!["rollback_delay"]),
            ("LATENCY = 7\n", vec!["latency"]),
            ("latency = 99\n", vec!["latency"]),
            ("rollback_delay = x\n", vec!["rollback_delay"]),
            (
                "rollback_delay = 5\nlatency = 6\n",
                vec!["rollback_delay", "latency"],
            ),
        ] {
            let s = read(extra);
            assert_eq!(
                delays(&s),
                (1, true, 2, true),
                "{extra:?}: 古いキーを読んだ"
            );
            assert_eq!(s.old_keys, want, "{extra:?}");
            let notes = s.old_key_notes();
            assert_eq!(notes.len(), want.len(), "{extra:?}");
            for (n, k) in notes.iter().zip(&want) {
                assert_eq!(n, &format!("古いキー {k} は読まない。delay を使う"));
            }
        }
        // 古いキーと新しいキーが両方あれば、新しいキーだけ。
        for (extra, n) in [
            ("latency = 6\nrollback_delay = 5\ndelay = 3\n", 3),
            ("delay = 0\nlatency = 6\n", 0),
        ] {
            let s = read(extra);
            assert_eq!(delays(&s), (n, false, n, false), "{extra:?}");
            assert!(!s.old_keys.is_empty(), "{extra:?}");
        }
        let s = read("latency = 6\nrollback_delay = 5\ndelay = auto\n");
        assert_eq!(delays(&s), (1, true, 2, true));
        // 古いキーを読み捨てても、新しいキーの誤りは注になる。
        let s = read("latency = 2\ndelay = 9\n");
        assert_eq!(
            (note_keys(&s), &s.old_keys),
            (vec!["delay"], &vec!["latency"])
        );
        // 同じ形の綴り違いは知らないキーとして注にする（古いキーの読み捨ては 2 つの名前だけ）。
        for (typo, key) in [
            ("latancy = 2\n", "latancy"),
            ("rollback_dalay = 1\n", "rollback_dalay"),
            ("delays = 1\n", "delays"),
        ] {
            let s = read(typo);
            assert_eq!(
                (note_keys(&s), s.old_keys.len()),
                (vec![key], 0),
                "{typo:?}"
            );
        }
    }

    /// 壊れたら落ちる: `lan_host` が既定で空にならない、LAN のアドレスとポートが読めない、決まりの外の値（LAN の外・形が違う）を黙って使う
    /// （使わずに注を残し、設定の残りは動かす）。
    #[test]
    fn lan_host_reads_a_lan_address_and_notes_anything_else() {
        assert_eq!(from_text(OK).unwrap().unwrap().lan_host, None);
        let a = from_text(&format!("{OK}lan_host = 192.168.0.2\n"))
            .unwrap()
            .unwrap();
        assert_eq!(
            a.lan_host,
            Some(LanAddr {
                ip: std::net::Ipv4Addr::new(192, 168, 0, 2),
                port: DEFAULT_BIND_PORT
            })
        );
        let b = from_text(&format!("{OK}lan_host = 10.1.2.3:19000\n"))
            .unwrap()
            .unwrap();
        assert_eq!(b.lan_host.map(|x| x.port), Some(19000));
        for bad in ["8.8.8.8", "192.168.0", "192.168.0.2:0", "host.example", "x"] {
            let s = from_text(&format!("{OK}lan_host = {bad}\n"))
                .unwrap()
                .unwrap();
            assert_eq!(s.lan_host, None, "{bad}");
            assert_eq!(note_keys(&s), vec!["lan_host"], "{bad}");
        }
        // 空の行は書かないのと同じ（注も出さない）。
        let s = from_text(&format!("{OK}lan_host =\n")).unwrap().unwrap();
        assert!((s.lan_host, s.ini_notes.len()) == (None, 0));
        assert_eq!(DEFAULT_BIND_PORT, higanr_core::lan_addr::DEFAULT_PORT);
    }

    #[test]
    fn head_wait_defaults_on_and_reads_zero_and_one_only() {
        assert!(from_text(OK).unwrap().unwrap().head_wait, "既定は有効");
        let on = from_text(&format!("{OK}head_wait = 1\n")).unwrap().unwrap();
        let off = from_text(&format!("{OK}head_wait = 0\n")).unwrap().unwrap();
        assert!(on.head_wait && !off.head_wait);
        assert_eq!(
            NetSettings {
                head_wait: true,
                ..off
            },
            on
        );
        for (bad, key) in [
            ("head_wait = 2", "head_wait"),
            ("head_wait = yes", "head_wait"),
            ("head_wait =", "head_wait"),
            ("head_wiat = 1", "head_wiat"),
        ] {
            let s = from_text(&format!("{OK}{bad}\n")).unwrap().unwrap();
            assert!(s.head_wait, "{bad}: 読めない行は既定の有効");
            assert_eq!(note_keys(&s), vec![key], "{bad}");
        }
    }

    #[test]
    fn rollback_hit_se_defaults_to_confirmed_and_reads_the_two_spellings_only() {
        assert_eq!(
            from_text(OK).unwrap().unwrap().rollback_hit_se,
            HitSe::Confirmed,
            "既定は確定まで待つ"
        );
        let c = from_text(&format!("{OK}rollback_hit_se = confirmed\n"))
            .unwrap()
            .unwrap();
        let l = from_text(&format!("{OK}rollback_hit_se = live\n"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (c.rollback_hit_se, l.rollback_hit_se),
            (HitSe::Confirmed, HitSe::Live)
        );
        assert_eq!(
            NetSettings {
                rollback_hit_se: HitSe::Confirmed,
                ..l
            },
            c
        );
        for (bad, key) in [
            ("rollback_hit_se =", "rollback_hit_se"),
            ("rollback_hit_se = Live", "rollback_hit_se"),
            ("rollback_hit_se = on", "rollback_hit_se"),
            ("rollback_hit_se = 1", "rollback_hit_se"),
            ("rollback_hit_sd = live", "rollback_hit_sd"),
        ] {
            let s = from_text(&format!("{OK}{bad}\n")).unwrap().unwrap();
            assert_eq!(s.rollback_hit_se, HitSe::Confirmed, "{bad}: 既定");
            assert_eq!(note_keys(&s), vec![key], "{bad}");
        }
    }

    #[test]
    fn refuses_unknown_keys_and_non_loopback() {
        let s = from_text(&format!("{OK}latancy = 3\n")).unwrap().unwrap();
        assert_eq!(note_keys(&s), vec!["latancy"]);
        for bad in [
            OK.replace("peer = 127.0.0.1:7002", "peer = 192.168.1.2:7002"),
            OK.replace("peer = 127.0.0.1:7002", "peer = localhost:7002"),
            OK.replace("bind = 127.0.0.1:0", "bind = 0.0.0.0:0"),
            OK.replace("peer = 127.0.0.1:7002", "peer = [::]:7002"),
            format!("{OK}after_script = 1\n"),
            format!("{OK}after_script = Keep\n"),
            format!("{OK}wait_budget_us = 0\n"),
            format!("{OK}lat_plan = 600\n"),
            format!("{OK}lat_plan = 600:10\n"),
            format!("{OK}lat_plan = 600:4 600:2\n"),
            format!("{OK}lat_plan = 900:4 600:2\n"),
            format!("{OK}lat_plan = x:4\n"),
            format!(
                "{OK}lat_plan = {}\n",
                (1..=17)
                    .map(|i| format!("{i}:1"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            OK.replace("0x48475254", "0x1_0000_0000"),
            OK.replace("0x48475254", "0x100000000"),
        ] {
            assert!(from_text(&bad).is_err(), "通した: {bad}");
        }
    }

    #[test]
    fn scope_widens_the_gate_only_as_written() {
        let s = from_text(OK).unwrap().unwrap();
        assert_eq!(s.scope, Scope::Loopback, "書かないときの既定");
        assert!(
            s.text().starts_with("ネットワークの範囲 loopback / "),
            "{}",
            s.text()
        );
        let lan = "[netplay]\nenable = 1\nscope = lan\nbind = 192.168.1.5:47001\n\
                   peer = 192.168.1.6:47002 10.0.0.7:47002 172.16.0.8:47002 [fd00::9]:47002 127.0.0.1:47002\n\
                   session = 1\nstun = 169.254.1.1:3478\n";
        let s = from_text(lan).unwrap().unwrap();
        assert_eq!(s.scope, Scope::Lan);
        assert_eq!(s.bind, Bind::Fixed("192.168.1.5:47001".parse().unwrap()));
        assert_eq!(s.peer_addrs().len(), 5);
        assert_eq!(
            s.stun,
            Some(StunTarget::Addr("169.254.1.1:3478".parse().unwrap()))
        );
        assert!(
            s.text().starts_with("ネットワークの範囲 lan / "),
            "{}",
            s.text()
        );
        // 同じ本文で範囲を書かない・loopback と書くと、私設のアドレスを断る（lan の行が効いている証拠）。
        for text in [
            lan.replace("scope = lan\n", ""),
            lan.replace("scope = lan", "scope = loopback"),
        ] {
            let e = from_text(&text).unwrap_err();
            assert!(e.contains("scope = loopback"), "{e}");
        }
        let inet = "[netplay]\nenable = 1\nscope = internet\nbind = 192.168.1.5:47001\n\
                    peer = 203.0.113.7:47002 [2001:db8::7]:47002\nsession = 1\nstun = 198.51.100.1:3478\n";
        let s = from_text(inet).unwrap().unwrap();
        assert_eq!(s.scope, Scope::Internet);
        assert_eq!(s.peer, Some("203.0.113.7:47002".parse().unwrap()));
        assert_eq!(
            s.stun,
            Some(StunTarget::Addr("198.51.100.1:3478".parse().unwrap()))
        );
        for bad in [
            lan.replace("10.0.0.7:47002", "203.0.113.7:47002"),
            lan.replace("stun = 169.254.1.1:3478", "stun = 198.51.100.1:3478"),
            lan.replace("bind = 192.168.1.5:47001", "bind = 203.0.113.5:47001"),
            // lan の区切りのすぐ外（172.16/12 の外の隣）
            lan.replace("172.16.0.8:47002", "172.32.0.8:47002"),
            inet.replace("bind = 192.168.1.5:47001", "bind = 0.0.0.0:47001"),
            inet.replace("bind = 192.168.1.5:47001", "bind = [::]:47001"),
            inet.replace("203.0.113.7:47002", "224.0.0.1:47002"),
            inet.replace("203.0.113.7:47002", "255.255.255.255:47002"),
            inet.replace("203.0.113.7:47002", "[::ffff:203.0.113.7]:47002"),
            inet.replace("203.0.113.7:47002", "203.0.113.7:0"),
            inet.replace("stun = 198.51.100.1:3478", "stun = [ff02::1]:3478"),
        ] {
            assert!(refused(&bad), "通した: {bad}");
        }
        // 読めない scope は loopback に戻し、その行だけ注にする。
        for v in [
            "LAN", "Lan", "Loopback", "wan", "local", "any", "lan lan", "",
        ] {
            let s = from_text(&OK.replace("enable = 1\n", &format!("enable = 1\nscope = {v}\n")))
                .unwrap()
                .unwrap();
            assert_eq!(
                (s.scope, note_keys(&s)),
                (Scope::Loopback, vec!["scope"]),
                "{v:?}"
            );
        }
    }

    #[test]
    fn bind_auto_reads_only_a_port() {
        for scope in ["loopback", "lan", "internet"] {
            let text = OK.replace(
                "bind = 127.0.0.1:0",
                &format!("scope = {scope}\nbind = auto:47001"),
            );
            // loopback 以外では peer が 127.0.0.1 でも通る（loopback は範囲の中）。
            let s = from_text(&text).unwrap().unwrap();
            assert_eq!(s.bind, Bind::Auto(47001), "{scope}");
            assert_eq!(s.bind.fixed(), None);
            assert!(s.text().contains("auto:47001（開くときに"), "{}", s.text());
        }
        let edge = from_text(&OK.replace("127.0.0.1:0", "auto:65535"))
            .unwrap()
            .unwrap();
        assert_eq!(edge.bind, Bind::Auto(65535));
        for bad in [
            "auto",
            "auto:",
            "auto:0",
            "auto:00",
            "auto:65536",
            "auto:99999999999",
            "auto:-1",
            "auto:+47001",
            "auto:47 001",
            "auto:0x10",
            "auto:abc",
            "auto:127.0.0.1:47001",
            "auto47001",
            "Auto:47001",
            "0.0.0.0:47001",
        ] {
            let text = OK.replace("127.0.0.1:0", bad);
            assert!(from_text(&text).is_err(), "通した: bind = {bad}");
        }
        let e = from_text(&OK.replace(
            "bind = 127.0.0.1:0",
            "scope = lan\nbind = 203.0.113.5:47001",
        ))
        .unwrap_err();
        assert!(e.contains("scope = lan"), "{e}");
    }

    #[test]
    fn bind_auto_resolves_by_scope_and_refuses_outside_it() {
        let ip = |s: &str| -> Result<IpAddr, String> { Ok(s.parse().unwrap()) };
        let never = || -> Result<IpAddr, String> { panic!("経路を引いた") };
        let (a, note) = Bind::Auto(47001).resolve(Scope::Loopback, never).unwrap();
        assert_eq!(a, "127.0.0.1:47001".parse().unwrap());
        assert!(
            note.contains("auto:47001") && note.contains("127.0.0.1:47001"),
            "{note}"
        );
        let fixed: SocketAddr = "192.168.1.9:47003".parse().unwrap();
        assert_eq!(
            Bind::Fixed(fixed).resolve(Scope::Lan, never),
            Ok((fixed, String::new()))
        );
        for (scope, route, ok) in [
            (Scope::Lan, "192.168.1.5", true),
            (Scope::Lan, "10.0.0.7", true),
            (Scope::Lan, "172.16.0.8", true),
            (Scope::Lan, "169.254.1.2", true),
            (Scope::Lan, "203.0.113.5", false),
            // CGNAT（100.64/10）が PC のアドレスの回線: lan には入れず、internet では通す（`guard.rs` の reach）。
            (Scope::Lan, "100.64.0.1", false),
            (Scope::Lan, "172.32.0.8", false),
            (Scope::Lan, "127.0.0.1", false),
            (Scope::Lan, "0.0.0.0", false),
            (Scope::Lan, "224.0.0.1", false),
            (Scope::Lan, "fd00::5", false),
            (Scope::Internet, "203.0.113.5", true),
            (Scope::Internet, "192.168.1.5", true),
            (Scope::Internet, "100.64.0.1", true),
            (Scope::Internet, "127.0.0.1", false),
            (Scope::Internet, "0.0.0.0", false),
            (Scope::Internet, "2001:db8::5", false),
        ] {
            let r = Bind::Auto(47001).resolve(scope, || ip(route));
            match (ok, r) {
                (true, Ok((a, note))) => {
                    assert_eq!(
                        a,
                        SocketAddr::new(route.parse().unwrap(), 47001),
                        "{scope:?} {route}"
                    );
                    assert!(
                        note.contains("auto:47001") && note.contains(&a.to_string()),
                        "{note}"
                    );
                    assert!(note.contains(scope.name()), "{note}");
                }
                (false, Err(e)) => assert!(
                    e.contains("bind にアドレスを手で書いてください") && e.contains("auto:47001"),
                    "{scope:?} {route}: {e}"
                ),
                (ok, r) => panic!("{scope:?} {route}: 期待 {ok} 実際 {r:?}"),
            }
        }
        let e = Bind::Auto(47001)
            .resolve(Scope::Lan, || ip("203.0.113.5"))
            .unwrap_err();
        assert!(
            e.contains("scope = lan") && e.contains("scope = internet"),
            "{e}"
        );
        let e = Bind::Auto(47001)
            .resolve(Scope::Lan, || Err("ネットワークに届かない".to_string()))
            .unwrap_err();
        assert!(
            e.contains("ネットワークに届かない") && e.contains("手で書いてください"),
            "{e}"
        );
    }

    #[test]
    fn stun_names_are_read_by_shape_without_resolving() {
        for scope in ["loopback", "lan", "internet"] {
            let text = OK.replace(
                "bind = 127.0.0.1:0",
                &format!("scope = {scope}\nbind = auto:47001"),
            ) + "stun = stun.cloudflare.com:3478\n";
            let s = from_text(&text).unwrap().unwrap();
            assert_eq!(
                s.stun,
                Some(StunTarget::Name {
                    host: "stun.cloudflare.com".to_string(),
                    port: 3478
                }),
                "{scope}"
            );
            assert!(
                s.text()
                    .contains("STUN stun.cloudflare.com:3478（開くときに名前を引く）"),
                "{}",
                s.text()
            );
        }
        let s = from_text(&format!("{OK}stun = localhost:65535\n"))
            .unwrap()
            .unwrap();
        assert_eq!(
            s.stun,
            Some(StunTarget::Name {
                host: "localhost".to_string(),
                port: 65535
            })
        );
        for bad in [
            "stun.example.net",
            "stun.example.net:",
            ":3478",
            "stun.example.net:0",
            "stun.example.net:65536",
            "stun.example.net:99999",
            "stun.example.net:-1",
            "stun.example.net:34 78",
            "stun.example.net:0x0d96",
            "st un.example.net:3478",
            "stun_1.example.net:3478",
            "stun.example.net.:3478",
            "stun..example.net:3478",
            "-stun.example.net:3478",
            "1.2.3:3478",
            "256.1.1.1:3478",
            "スタン.example.net:3478",
            "http://stun.example.net:3478",
            "stun.example.net:3478/x",
            "stun.example.net:3478:3478",
            format!("{}.example:3478", "a".repeat(64)).as_str(),
            "10.0.0.1:3478",
            "203.0.113.1:3478",
            "127.0.0.1:0",
            "[::]:3478",
        ] {
            let s = from_text(&format!("{OK}stun = {bad}\n")).unwrap().unwrap();
            assert_eq!(
                (&s.stun, note_keys(&s)),
                (&None, vec!["stun"]),
                "stun = {bad}"
            );
        }
    }

    #[test]
    fn the_friend_form_with_auto_bind_and_a_named_stun_reads() {
        for scope in ["lan", "internet"] {
            let text = format!(
                "[netplay]\nenable = 1\nscope = {scope}\nbind = auto:47001\ncode_side = offer\nstun = stun.cloudflare.com:3478\n"
            );
            let s = from_text(&text).unwrap().unwrap();
            assert_eq!(s.bind, Bind::Auto(47001));
            assert_eq!(s.code_side, Some(CodeSide::Offer));
            assert!(matches!(s.stun, Some(StunTarget::Name { port: 3478, .. })));
            assert_eq!(s.scope.name(), scope);
        }
    }

    #[test]
    fn ipv6_is_on_by_default_and_only_on_and_off_are_read() {
        let on = from_text(OK).unwrap().unwrap();
        assert!(on.ipv6, "既定は on");
        assert!(
            from_text(&format!("{OK}ipv6 = on\n"))
                .unwrap()
                .unwrap()
                .ipv6
        );
        let off = from_text(&format!("{OK}ipv6 = off\n")).unwrap().unwrap();
        assert!(!off.ipv6);
        assert_eq!(
            NetSettings {
                ipv6: true,
                ..off.clone()
            },
            on,
            "他の欄が変わった"
        );
        for bad in ["ON", "Off", "", "1", "0", "yes", "true", "auto"] {
            let s = from_text(&format!("{OK}ipv6 = {bad}\n")).unwrap().unwrap();
            assert!(s.ipv6, "ipv6 = {bad:?}: 読めない行は既定の on");
            assert_eq!(note_keys(&s), vec!["ipv6"], "ipv6 = {bad:?}");
        }
        assert!(
            off.text().contains("IPv6 の経路 使わない"),
            "{}",
            off.text()
        );
        assert!(on.text().contains("IPv6 の経路 使う"), "{}", on.text());
    }

    #[test]
    fn upnp_is_on_by_default_and_only_on_and_off_are_read() {
        let on = from_text(OK).unwrap().unwrap();
        assert!(on.upnp, "既定は on");
        assert!(
            from_text(&format!("{OK}upnp = on\n"))
                .unwrap()
                .unwrap()
                .upnp
        );
        let off = from_text(&format!("{OK}upnp = off\n")).unwrap().unwrap();
        assert!(!off.upnp);
        assert_eq!(
            NetSettings {
                upnp: true,
                ..off.clone()
            },
            on,
            "他の欄が変わった"
        );
        for bad in ["ON", "Off", "", "1", "0", "yes", "true", "auto"] {
            let s = from_text(&format!("{OK}upnp = {bad}\n")).unwrap().unwrap();
            assert!(s.upnp, "upnp = {bad:?}: 読めない行は既定の on");
            assert_eq!(note_keys(&s), vec!["upnp"], "upnp = {bad:?}");
        }
        assert!(off.text().contains("UPnP 頼まない"), "{}", off.text());
        assert!(on.text().contains("UPnP 頼む"), "{}", on.text());
    }

    /// UPnP で割り当てた外側のポートは「STUN で見えた外側のアドレス ＋ そのポート」として載る。載らない場合: 割り当てが無い・外側のアドレスが
    /// 分からない・外側のアドレスが IPv6・同じ候補が既にある。
    #[test]
    fn the_upnp_port_joins_the_candidates_on_the_mapped_address_only() {
        let local: SocketAddr = "192.168.0.5:47101".parse().unwrap();
        let seen: SocketAddr = "203.0.113.7:50001".parse().unwrap();
        let addrs = |c: Candidates| c.as_slice().iter().map(|c| c.addr).collect::<Vec<_>>();
        let sa = |s: &str| -> SocketAddr { s.parse().unwrap() };
        let all = from_text(OK).unwrap().unwrap();
        // 割り当てが無ければ今までの候補のまま。
        let base = addrs(all.candidates_for(local, Some(seen), None).unwrap());
        assert_eq!(
            addrs(
                all.candidates_for_upnp(local, Some(seen), None, None)
                    .unwrap()
            ),
            base
        );
        // 割り当てあり: 外側のアドレス ＋ 割り当てたポート。
        let got = addrs(
            all.candidates_for_upnp(local, Some(seen), None, Some(52_000))
                .unwrap(),
        );
        assert!(got.contains(&sa("203.0.113.7:52000")), "{got:?}");
        assert!(got.contains(&local) && got.contains(&seen), "{got:?}");
        assert_eq!(got.len(), base.len() + 1, "{got:?}");
        // 割り当てたポートがソケットと同じなら、ソケットのポートの候補（付け替え対策）と重ねない。
        let same = addrs(
            all.candidates_for_upnp(local, Some(seen), None, Some(47_101))
                .unwrap(),
        );
        assert_eq!(same, base, "重なった");
        // 割り当てたポートが STUN で見えたポートと同じなら重ねない。
        let same_as_seen = addrs(
            all.candidates_for_upnp(local, Some(seen), None, Some(50_001))
                .unwrap(),
        );
        assert_eq!(same_as_seen, base, "重なった");
        // 外側のアドレスが分からない・IPv6 なら載せない（アドレスが無いので、ポートだけの候補は作れない）。
        assert_eq!(
            addrs(
                all.candidates_for_upnp(local, None, None, Some(52_000))
                    .unwrap()
            ),
            vec![local]
        );
        let seen6: SocketAddr = "[2400:cb00::7]:50001".parse().unwrap();
        assert!(
            !addrs(
                all.candidates_for_upnp(local, Some(seen6), None, Some(52_000))
                    .unwrap()
            )
            .iter()
            .any(|a| a.port() == 52_000),
            "IPv6 の外側のアドレスにポートを足した"
        );
        // 候補の上限（8）の中に収まる。
        let g6: SocketAddr = "[2400:cb00::7]:47101".parse().unwrap();
        let full = all
            .candidates_for_upnp(local, Some(seen), Some(g6), Some(52_000))
            .unwrap();
        assert!(full.len() <= higanr_core::candidate::MAX_CANDIDATES);
        // `candidates = mapped`（ソケットを載せない開発用）でも載る。
        let m = from_text(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = offer\nstun = 127.0.0.1:3478\ncandidates = mapped\n",
        )
        .unwrap()
        .unwrap();
        let got = addrs(
            m.candidates_for_upnp(local, Some(seen), None, Some(52_000))
                .unwrap(),
        );
        assert!(got.contains(&sa("203.0.113.7:52000")), "{got:?}");
        assert!(!got.contains(&local), "受信ソケットのアドレスは載せない");
    }

    #[test]
    fn own_candidates_carry_a_global_ipv6_only_and_the_code_reads_back() {
        let local: SocketAddr = "192.168.0.5:47101".parse().unwrap();
        let seen: SocketAddr = "203.0.113.7:50001".parse().unwrap();
        let g6: SocketAddr = "[2400:cb00::7]:47101".parse().unwrap();
        // 外側のアドレスのポートをソケットのポートに替えたアドレス（ルーターが付け替えたときに開けたポートを指す）。
        let punch: SocketAddr = "203.0.113.7:47101".parse().unwrap();
        let addrs = |c: Candidates| c.as_slice().iter().map(|x| x.addr).collect::<Vec<_>>();
        let all = from_text(OK).unwrap().unwrap();
        assert_eq!(
            addrs(all.candidates_for(local, Some(seen), Some(g6)).unwrap()),
            [g6, local, seen, punch],
            "優先の順（IPv6 → LAN → 外側）"
        );
        assert_eq!(
            addrs(all.candidates_for(local, Some(seen), None).unwrap()),
            [local, seen, punch],
            "IPv6 が無いときはアドレスとポートの候補だけ"
        );
        for bad in [
            "[2001::7]:47101",
            "[2002::7]:47101",
            "[2001:db8::7]:47101",
            "[fe80::7]:47101",
            "[fd00::7]:47101",
            "[::1]:47101",
            "203.0.113.9:47101",
        ] {
            let b: SocketAddr = bad.parse().unwrap();
            assert_eq!(
                addrs(all.candidates_for(local, Some(seen), Some(b)).unwrap()),
                [local, seen, punch],
                "載せてはいけない {bad} を載せた"
            );
        }
        // `candidates = mapped`: ソケット（家の中）は載せず、世界に通じる IPv6 は載せる。
        let m = from_text(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = offer\nstun = 127.0.0.1:3478\ncandidates = mapped\n"
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            addrs(m.candidates_for(local, Some(seen), Some(g6)).unwrap()),
            [g6, seen, punch]
        );
        assert_eq!(
            addrs(m.candidates_for(local, None, Some(g6)).unwrap()),
            [g6]
        );
        assert!(m.candidates_for(local, None, None).is_err());
        assert!(
            m.candidates_for(local, None, Some("[fe80::7]:47101".parse().unwrap()))
                .is_err(),
            "載せられない IPv6 だけでは候補が空になる"
        );
        let secret = Secret([7; 16]);
        let t = all
            .initiator_code(secret, 1, local, Some(seen), Some(g6), 1_000)
            .unwrap();
        let c = code::read_initiator(&t, 1_000).unwrap();
        assert_eq!(addrs(c.candidates), [g6, local, seen, punch]);
        assert_eq!(
            c.candidates.as_slice().first().map(|x| x.kind),
            Some(higanr_core::candidate::CandKind::Ipv6)
        );
    }

    #[test]
    fn a_moved_port_adds_the_local_port_on_the_mapped_ip_and_the_code_reads_back() {
        let local: SocketAddr = "192.168.0.5:17936".parse().unwrap();
        let seen: SocketAddr = "203.0.113.7:50001".parse().unwrap();
        let punch: SocketAddr = "203.0.113.7:17936".parse().unwrap();
        let g6: SocketAddr = "[2400:cb00::7]:17936".parse().unwrap();
        let addrs = |c: Candidates| c.as_slice().iter().map(|x| x.addr).collect::<Vec<_>>();
        let all = from_text(OK).unwrap().unwrap();
        // 並びは push の優先（IPv6 → LAN → 外側）で決まる。足したアドレスは外側のアドレスの後ろ。
        assert_eq!(
            addrs(all.candidates_for(local, Some(seen), Some(g6)).unwrap()),
            [g6, local, seen, punch]
        );
        assert_eq!(
            addrs(all.candidates_for(local, Some(seen), None).unwrap()),
            [local, seen, punch],
            "ポートが違うとき、アドレス＋受信ソケットのポートを足す"
        );
        let same: SocketAddr = "203.0.113.7:17936".parse().unwrap();
        assert_eq!(
            addrs(all.candidates_for(local, Some(same), None).unwrap()),
            [local, same],
            "ポートが同じときは足さない"
        );
        assert_eq!(
            addrs(all.candidates_for(local, None, None).unwrap()),
            [local],
            "mapped が無いときは足さない"
        );
        let seen6: SocketAddr = "[2400:cb00::9]:50001".parse().unwrap();
        assert_eq!(
            addrs(all.candidates_for(local, Some(seen6), None).unwrap()),
            [seen6, local],
            "mapped が IPv6 のときは足さない"
        );
        // `candidates = mapped`: ソケットは載せないまま、アドレス＋ソケットのポートは足す。
        let m = from_text(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = offer\nstun = 127.0.0.1:3478\ncandidates = mapped\n"
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            addrs(m.candidates_for(local, Some(seen), Some(g6)).unwrap()),
            [g6, seen, punch]
        );
        assert_eq!(
            addrs(m.candidates_for(local, Some(seen), None).unwrap()),
            [seen, punch]
        );
        let secret = Secret([7; 16]);
        let t = all
            .initiator_code(secret, 1, local, Some(seen), None, 1_000)
            .unwrap();
        assert_eq!(
            addrs(code::read_initiator(&t, 1_000).unwrap().candidates),
            [local, seen, punch]
        );
        let t = m
            .initiator_code(secret, 1, local, Some(seen), None, 1_000)
            .unwrap();
        assert_eq!(
            addrs(code::read_initiator(&t, 1_000).unwrap().candidates),
            [seen, punch]
        );
    }

    #[test]
    fn stun2_is_read_like_stun_and_empty_means_not_asked() {
        let name = |v: &str| StunTarget::Name {
            host: v.rsplit_once(':').unwrap().0.to_string(),
            port: v.rsplit_once(':').unwrap().1.parse().unwrap(),
        };
        assert_eq!(from_text(OK).unwrap().unwrap().stun2, None);
        assert_eq!(
            from_text(&format!("{OK}stun2 =\n")).unwrap().unwrap().stun2,
            None
        );
        let s = from_text(&format!(
            "{OK}stun = 127.0.0.1:3478\nstun2 = stun.l.google.com:19302\n"
        ))
        .unwrap()
        .unwrap();
        assert_eq!(s.stun2, Some(name("stun.l.google.com:19302")));
        assert!(
            s.text()
                .contains("2 か所目の STUN stun.l.google.com:19302（開くときに名前を引く）"),
            "{}",
            s.text()
        );
        assert!(
            from_text(OK)
                .unwrap()
                .unwrap()
                .text()
                .contains("2 か所目の STUN 聞かない"),
            "書かない既定の文"
        );
        let s = from_text(&format!("{OK}stun2 = 203.0.113.9:3478\n"))
            .unwrap()
            .unwrap();
        assert_eq!((&s.stun2, note_keys(&s)), (&None, vec!["stun2"]));
        let wide = OK.replace("bind = 127.0.0.1:0", "scope = internet\nbind = 127.0.0.1:0");
        assert_eq!(
            from_text(&format!("{wide}stun2 = 203.0.113.9:3478\n"))
                .unwrap()
                .unwrap()
                .stun2,
            Some(StunTarget::Addr("203.0.113.9:3478".parse().unwrap()))
        );
        for bad in [
            "stun.l.google.com",
            "stun.l.google.com:0",
            "stun.l.google.com:70000",
            "stun_x.example:3478",
            "http://stun.l.google.com:19302",
        ] {
            let s = from_text(&format!("{OK}stun2 = {bad}\n")).unwrap().unwrap();
            assert_eq!((&s.stun2, note_keys(&s)), (&None, vec!["stun2"]), "{bad}");
        }
    }

    #[test]
    fn the_m2_sample_ini_reads_as_documented() {
        let text = include_str!("../../../scripts/m2_netplay.ini");
        let s = from_text(text)
            .unwrap()
            .expect("見本でネットワークが開かない");
        assert_eq!(s.bind, Bind::Fixed("127.0.0.1:47001".parse().unwrap()));
        assert_eq!(s.peer, Some("127.0.0.1:47002".parse().unwrap()));
        assert_eq!(s.session_id, Some(peer_sim::cli::DEFAULT_SESSION));
        assert_eq!(s.secret, Some(peer_sim::cli::DEFAULT_SECRET));
        assert!(s.extra_peers.is_empty() && s.stun.is_none());
        assert!(s.role_nonce.is_some_and(|n| n > 1));
        assert_eq!(
            (s.latency, s.host_seat, s.check),
            (2, Seat::P1, CheckSource::InputDigest)
        );
    }

    #[test]
    fn the_m6_3_watch_ini_is_a_spectator_that_keeps_measuring() {
        let s = from_text(include_str!("../../../scripts/m6_3_watch.ini"))
            .unwrap()
            .expect("見本でネットワークが開かない");
        assert_eq!(s.code_side, Some(CodeSide::Spectate));
        assert_eq!(s.after_script, AfterScript::Keep);
        assert_eq!(s.code_dir.as_deref(), Some("spec_watch"));
        let taken: [SocketAddr; 3] = [
            "127.0.0.1:47101".parse().unwrap(),
            "127.0.0.1:47102".parse().unwrap(),
            "127.0.0.1:47111".parse().unwrap(),
        ];
        assert!(matches!(s.bind, Bind::Fixed(a) if !taken.contains(&a)));
        assert!(s.peer.is_none() && s.scope == Scope::Loopback);
    }

    #[test]
    fn the_two_game_inis_mirror_each_other() {
        let h = from_text(include_str!("../../../scripts/two_host.ini"))
            .unwrap()
            .expect("ホストの見本でネットワークが開かない");
        let g = from_text(include_str!("../../../scripts/two_guest.ini"))
            .unwrap()
            .expect("ゲストの見本でネットワークが開かない");
        assert_eq!(h.bind, Bind::Fixed("127.0.0.1:47001".parse().unwrap()));
        assert_eq!(h.peer, Some("127.0.0.1:47002".parse().unwrap()));
        assert_eq!((g.bind.fixed(), g.peer), (h.peer, h.bind.fixed()));
        assert!(h.extra_peers.is_empty() && g.extra_peers.is_empty());
        assert!(h.secret.is_some());
        assert_eq!(
            (h.session_id, h.secret, h.latency, h.host_seat, h.check),
            (g.session_id, g.secret, g.latency, g.host_seat, g.check)
        );
        assert_eq!(h.host_seat, Seat::P1);
        let (hn, gn) = (h.role_nonce.unwrap(), g.role_nonce.unwrap());
        assert!(
            hn > gn,
            "ホストの nonce {hn} がゲストの {gn} より大きくない"
        );
        assert!(h.lat_plan.is_empty() && g.lat_plan.is_empty());
        assert!(h.stun.is_none() && g.stun.is_none());
    }

    #[test]
    fn code_side_reads_offer_and_answer() {
        let base = "[netplay]\nenable = 1\nbind = 127.0.0.1:0\n";
        // 「ネットワーク: 開いた」の行のモードの欄: ウィンドウで選べる実行（offer / answer）にだけ「後の接続コードの行が勝つ」が付く。
        const NOTE: &str = "接続コードのウィンドウで選べば、後の接続コードの行が勝つ";
        for side in ["offer", "answer"] {
            let s = from_text(&format!("{base}code_side = {side}\n"))
                .unwrap()
                .unwrap();
            assert!(s.text().contains(NOTE), "{side}: {}", s.text());
        }
        let plain = from_text(&format!("{base}peer = 127.0.0.1:7002\nsession = 0x10\n"))
            .unwrap()
            .unwrap();
        assert!(!plain.text().contains(NOTE), "{}", plain.text());
        let o = from_text(&format!("{base}code_side = offer\n"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                o.code_side,
                o.peer,
                o.session_id,
                o.secret,
                o.code_dir.clone()
            ),
            (Some(CodeSide::Offer), None, None, None, None)
        );
        assert!(o.peer_addrs().is_empty());
        let o2 = from_text(&format!(
            "{base}code_side = offer\nsession = 0x10\nsecret = 000102030405060708090a0b0c0d0e0f\ncode_dir = code_a\n"
        ))
        .unwrap()
        .unwrap();
        assert_eq!(
            (o2.session_id, o2.code_dir.as_deref()),
            (Some(0x10), Some("code_a"))
        );
        assert!(o2.secret.is_some());
        let a = from_text(&format!("{base}code_side = answer\n"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (a.code_side, a.peer, a.session_id, a.secret),
            (Some(CodeSide::Answer), None, None, None)
        );
        for bad in [
            format!("{base}code_side = host\n"),
            format!("{base}code_side = guest\n"),
            format!("{base}code_side = Offer\n"),
            format!("{base}code_role = host\n"),
            format!("{base}code_side = offer\npeer = 127.0.0.1:7002\n"),
            format!("{base}code_side = answer\nsession = 1\n"),
            format!("{base}code_side = answer\nsecret = 000102030405060708090a0b0c0d0e0f\n"),
            format!("{base}code_side = offer\ncode_dir =\n"),
            format!("{OK}code_dir = code_a\n"),
            format!("{base}session = 1\n"),
        ] {
            assert!(from_text(&bad).is_err(), "通した: {bad}");
        }
    }

    #[test]
    fn the_code_inis_pair_up() {
        let o = from_text(include_str!("../../../scripts/code_offer.ini"))
            .unwrap()
            .expect("始める側の見本でネットワークが開かない");
        let a = from_text(include_str!("../../../scripts/code_answer.ini"))
            .unwrap()
            .expect("返す側の見本でネットワークが開かない");
        assert_eq!(
            (o.code_side, a.code_side),
            (Some(CodeSide::Offer), Some(CodeSide::Answer))
        );
        assert_ne!(o.code_dir, a.code_dir, "置き場が同じ");
        assert!(o.code_dir.is_some() && a.code_dir.is_some());
        for s in [&o, &a] {
            assert!(s.peer.is_none() && s.session_id.is_none() && s.secret.is_none());
            assert_eq!(
                s.bind.fixed().map(|a| a.port()),
                Some(0),
                "受信ソケットのポートを決め打ちしている"
            );
        }
        assert!(o.role_nonce.unwrap() > a.role_nonce.unwrap());
        assert_eq!(
            (o.latency, o.host_seat, o.check, o.after_script),
            (a.latency, a.host_seat, a.check, a.after_script)
        );
    }

    #[test]
    fn candidates_key_reads_all_or_mapped_only_with_code_side() {
        let base = "[netplay]\nenable = 1\nscope = internet\nbind = auto:47101\ncode_side = offer\nstun = stun.cloudflare.com:3478\n";
        let read = |extra: &str| from_text(&format!("{base}{extra}")).map(|o| o.unwrap());
        assert_eq!(read("").unwrap().candidates, OwnCandidates::All);
        assert_eq!(
            read("candidates = all\n").unwrap().candidates,
            OwnCandidates::All
        );
        let m = read("candidates = mapped\n").unwrap();
        assert_eq!(m.candidates, OwnCandidates::Mapped);
        assert!(
            m.text().contains("STUN で見えたアドレスだけ"),
            "{}",
            m.text()
        );
        for bad in [
            "candidates = Mapped\n",
            "candidates = local\n",
            "candidates = \n",
            "candidates = mapped all\n",
        ] {
            assert!(read(bad).is_err(), "{bad:?} を通した");
        }
        for v in ["all", "mapped"] {
            let e = from_text(&format!(
                "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 1\nstun = 127.0.0.1:3478\ncandidates = {v}\n"
            ))
            .unwrap_err();
            assert!(e.contains("code_side と一緒にだけ"), "{v}: {e}");
        }
        let no_stun = "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = offer\n";
        for extra in ["candidates = mapped\n", "candidates = mapped\nstun =\n"] {
            let e = from_text(&format!("{no_stun}{extra}")).unwrap_err();
            assert!(e.contains("stun が要る"), "{extra:?}: {e}");
        }
        assert!(from_text(&format!("{no_stun}candidates = all\n")).is_ok());

        // 自分の候補の作り。ソケット `local` は家の中のアドレス、`seen` は STUN で見えたアドレス（ポートも違う）。
        let (local, seen): (SocketAddr, SocketAddr) = (
            "192.168.0.5:47101".parse().unwrap(),
            "203.0.113.7:50001".parse().unwrap(),
        );
        let addrs = |c: Candidates| c.as_slice().iter().map(|x| x.addr).collect::<Vec<_>>();
        let punch: SocketAddr = "203.0.113.7:47101".parse().unwrap();
        let all = read("").unwrap();
        assert_eq!(
            addrs(all.candidates_for(local, Some(seen), None).unwrap()),
            [local, seen, punch]
        );
        assert_eq!(
            addrs(all.candidates_for(local, None, None).unwrap()),
            [local]
        );
        assert_eq!(
            addrs(m.candidates_for(local, Some(seen), None).unwrap()),
            [seen, punch]
        );
        let e = m.candidates_for(local, None, None).unwrap_err();
        assert!(
            e.contains("candidates = mapped") && e.contains("STUN"),
            "{e}"
        );
        let secret = Secret([7; 16]);
        let t = m
            .initiator_code(secret, 1, local, Some(seen), None, 1_000)
            .unwrap();
        assert_eq!(
            addrs(code::read_initiator(&t, 0).unwrap().candidates),
            [seen, punch]
        );
    }

    #[test]
    fn the_hairpin_inis_pair_up() {
        let o = from_text(include_str!("../../../scripts/hairpin_offer.ini"))
            .unwrap()
            .expect("始める側の見本でネットワークが開かない");
        let a = from_text(include_str!("../../../scripts/hairpin_answer.ini"))
            .unwrap()
            .expect("返す側の見本でネットワークが開かない");
        assert_eq!(
            (o.code_side, a.code_side),
            (Some(CodeSide::Offer), Some(CodeSide::Answer))
        );
        for s in [&o, &a] {
            assert_eq!(s.candidates, OwnCandidates::Mapped);
            assert!(!s.upnp, "実機の回で使う人のルーターを書き換えない");
            assert_eq!(s.scope, Scope::Internet);
            assert!(matches!(s.stun, Some(StunTarget::Name { port: 3478, .. })));
            assert!(s.peer.is_none() && s.session_id.is_none() && s.secret.is_none());
        }
        assert_eq!(o.stun, a.stun);
        assert_eq!((o.bind, a.bind), (Bind::Auto(47101), Bind::Auto(47102)));
        assert_ne!(o.code_dir, a.code_dir, "置き場が同じ");
        for d in [&o.code_dir, &a.code_dir] {
            let d = d.as_deref().unwrap();
            assert!(
                d != "code_a" && d != "code_b",
                "loopback の見本と置き場が同じ: {d}"
            );
        }
        assert!(o.role_nonce.unwrap() > a.role_nonce.unwrap());
        assert_eq!(
            (o.latency, o.host_seat, o.check, o.after_script),
            (a.latency, a.host_seat, a.check, a.after_script)
        );
        let dist = include_str!("../../../scripts/dist/HiganRTool.ini");
        assert!(
            !dist.contains("candidates"),
            "配る ini に candidates がある"
        );
    }

    #[test]
    fn the_lan_inis_pair_up() {
        let o = from_text(include_str!("../../../scripts/lan_offer.ini"))
            .unwrap()
            .expect("始める側の見本でネットワークが開かない");
        let a = from_text(include_str!("../../../scripts/lan_answer.ini"))
            .unwrap()
            .expect("返す側の見本でネットワークが開かない");
        assert_eq!(
            (o.code_side, a.code_side),
            (Some(CodeSide::Offer), Some(CodeSide::Answer))
        );
        for s in [&o, &a] {
            assert_eq!(s.candidates, OwnCandidates::All);
            assert_eq!(s.scope, Scope::Lan);
            assert_eq!(s.stun, None, "STUN を書かない（回 1 の 10049）");
            assert!(s.peer.is_none() && s.session_id.is_none() && s.secret.is_none());
        }
        assert_eq!((o.bind, a.bind), (Bind::Auto(47001), Bind::Auto(47002)));
        assert_ne!(o.code_dir, a.code_dir, "置き場が同じ");
        for d in [&o.code_dir, &a.code_dir] {
            let d = d.as_deref().unwrap();
            assert!(
                !["code_a", "code_b", "code_hairpin_a", "code_hairpin_b"].contains(&d),
                "ほかの回の見本と置き場が同じ: {d}"
            );
        }
        assert!(o.role_nonce.unwrap() > a.role_nonce.unwrap());
        assert_eq!(
            (o.latency, o.host_seat, o.check, o.after_script),
            (a.latency, a.host_seat, a.check, a.after_script)
        );
        // 両方で自動保存を入れる（片方だけ入っていると比べる相手が欠ける）。
        for text in [
            include_str!("../../../scripts/lan_offer.ini"),
            include_str!("../../../scripts/lan_answer.ini"),
        ] {
            let r = higanr_core::autosave::settings_from_ini(&ini::parse(text).unwrap())
                .unwrap()
                .expect("LAN の見本で自動保存が入らない");
            assert_eq!(r.split, higanr_core::vault::Split::Peer);
        }
    }

    #[test]
    fn the_m8_4_choose_inis_pair_up() {
        let read = |t: &str| from_text(t).unwrap().expect("ネットワークが開かない");
        let (ta, tb) = (
            include_str!("../../../scripts/m8_4_choose_a.ini"),
            include_str!("../../../scripts/m8_4_choose_b.ini"),
        );
        let (a, b) = (read(ta), read(tb));
        assert_eq!(
            (a.code_side, b.code_side),
            (Some(CodeSide::Offer), Some(CodeSide::Answer))
        );
        for s in [&a, &b] {
            assert_eq!(
                (s.mode, s.rollback_delay, s.rollback_ahead),
                (Mode::Rollback, 1, 8)
            );
            assert_eq!(s.scope, Scope::Lan);
        }
        assert!(a.role_nonce.unwrap() > b.role_nonce.unwrap());
        assert_eq!((a.bind, b.bind), (Bind::Auto(47001), Bind::Auto(47002)));
        assert_ne!(a.code_dir, b.code_dir);
        assert_ne!(a.name, b.name);
        assert_eq!(
            (a.latency, a.host_seat, a.check),
            (b.latency, b.host_seat, b.check)
        );
        for t in [ta, tb] {
            let ini = ini::parse(t).unwrap();
            assert!(
                !ini.has_section("replay"),
                "rollback の回に自動保存を入れた"
            );
        }
    }

    #[test]
    fn the_shape_inis_pair_up() {
        let (a_text, b_text) = (
            include_str!("../../../scripts/shape_a.ini"),
            include_str!("../../../scripts/shape_b.ini"),
        );
        let a = from_text(a_text)
            .unwrap()
            .expect("A の見本でネットワークが開かない");
        let b = from_text(b_text)
            .unwrap()
            .expect("B の見本でネットワークが開かない");
        let (a_bind, b_bind) = (a.bind.fixed().unwrap(), b.bind.fixed().unwrap());
        let (a_peer, b_peer) = (a.peer.unwrap(), b.peer.unwrap());
        assert_eq!(
            (a_bind, b_bind),
            (
                "127.0.0.1:47001".parse().unwrap(),
                "127.0.0.1:47002".parse().unwrap()
            )
        );
        let all = [a_bind, b_bind, a_peer, b_peer];
        for (i, x) in all.iter().enumerate() {
            assert!(!all[..i].contains(x), "アドレス {x} が重なる: {all:?}");
        }
        for s in [&a, &b] {
            assert_eq!(s.scope, Scope::Loopback);
            assert!(s.extra_peers.is_empty() && s.stun.is_none() && s.code_side.is_none());
            assert!(s.lat_plan.is_empty());
        }
        assert_eq!(
            (
                a.session_id,
                a.secret,
                a.latency,
                a.host_seat,
                a.check,
                a.after_script
            ),
            (
                b.session_id,
                b.secret,
                b.latency,
                b.host_seat,
                b.check,
                b.after_script
            )
        );
        assert!(a.secret.is_some() && a.session_id.is_some());
        assert_eq!(a.host_seat, Seat::P1);
        let (an, bn) = (a.role_nonce.unwrap(), b.role_nonce.unwrap());
        assert!(an > bn, "A の nonce {an} が B の {bn} より大きくない");
        for text in [a_text, b_text] {
            let r = higanr_core::autosave::settings_from_ini(&ini::parse(text).unwrap())
                .unwrap()
                .expect("中継越しの見本で自動保存が入らない");
            assert_eq!(r.split, higanr_core::vault::Split::Peer);
        }
        let relay = format!("--a {a_bind} --b {b_bind} --via-a {a_peer} --via-b {b_peer}");
        // 回の定義が A に shape_a.ini・B に shape_b.ini を指す（`runs_consistency` は 2 台の回の ini の取り違えを見ない）。
        for (run, ini_line) in [
            (
                include_str!("../../../scripts/runs/shape_1_a.run"),
                "ini = scripts/shape_a.ini",
            ),
            (
                include_str!("../../../scripts/runs/shape_1_b.run"),
                "ini = scripts/shape_b.ini",
            ),
        ] {
            assert!(
                run.lines().any(|l| l.trim() == ini_line),
                "回の定義に {ini_line} が無い"
            );
        }
        for (name, text) in [
            ("shape_a.ini", a_text),
            (
                "shape_1_a.run",
                include_str!("../../../scripts/runs/shape_1_a.run"),
            ),
        ] {
            assert!(
                text.contains(&relay),
                "{name} の中継の例が ini のアドレスとずれる: {relay}"
            );
        }
    }

    #[test]
    fn the_two_game_scripts_pair_up() {
        use higanr_core::input_model::keys;
        let (host, guest) = check_two_game_pair(
            include_str!("../../../scripts/two_01_host.txt"),
            include_str!("../../../scripts/two_01_guest.txt"),
            &[],
        );
        assert_eq!(match_tail(&host), match_tail(&guest));
        let taps = match_tail(&host).iter().filter(|t| t.2 == keys::Z).count();
        assert_eq!(taps, 18);
    }

    #[test]
    fn the_busy_two_game_scripts_press_many_words_and_pair_up() {
        use higanr_core::input_model::keys;
        let (host, guest) = check_two_game_pair(
            include_str!("../../../scripts/two_02_host.txt"),
            include_str!("../../../scripts/two_02_guest.txt"),
            &[],
        );
        let (h, g) = (match_tail(&host), match_tail(&guest));
        assert_eq!(h.len(), g.len());
        assert!(
            h.iter().zip(&g).all(|(a, b)| (a.0, a.1) == (b.0, b.1)),
            "待ち・押す長さが両者で食い違う"
        );
        // ワードの変わり目の数。wait（最低 1 tick）の間はワードが 0、hold の間はワードなので、押す段階 1 つにつき 0→ワード→0 の 2 回変わる。
        let changes = |words: &[(u32, u32, u16)]| words.iter().filter(|w| w.2 != 0).count() * 2;
        assert!(
            changes(&h) >= 50,
            "ホストのワードの変わり目 {}",
            changes(&h)
        );
        assert!(
            changes(&g) >= 50,
            "ゲストのワードの変わり目 {}",
            changes(&g)
        );
        assert_ne!(h, g, "両者のワードが全く同じ");
        let banned = keys::UP | keys::DOWN | keys::X | keys::ESC;
        for t in h.iter().chain(&g) {
            assert_eq!(t.2 & banned, 0, "試合の中で上・下・X・Esc を押す");
        }
        for (name, t) in [("ホスト", &h), ("ゲスト", &g)] {
            assert!(
                t.iter().filter(|x| x.2 & keys::Z != 0).count() >= 50,
                "{name}: Z を含む語が少ない"
            );
        }
        // マッチの長さは約 2〜3 分（1P の初期体力 1 のマッチが終わって終わりのメニューへ届く長さ）。
        let ticks: u32 = h.iter().map(|t| t.0 + t.1).sum();
        assert!((6000..=12000).contains(&ticks), "試合の中が {ticks} tick");
    }

    #[test]
    fn the_third_two_game_scripts_walk_pause_title_chara_change_and_rematches() {
        use higanr_core::input_model::keys;
        use higanr_core::script::Step;
        let (host, guest) = check_two_game_pair(
            include_str!("../../../scripts/two_03_host.txt"),
            include_str!("../../../scripts/two_03_guest.txt"),
            &["end_down"],
        );
        let st = &host.steps;
        let at = |from: usize, f: &dyn Fn(&Step) -> bool| -> usize {
            from + st[from..]
                .iter()
                .position(f)
                .unwrap_or_else(|| panic!("{from} 段階目より後に、探している段階が無い"))
        };
        let paused = |s: &Step| s.cond.pause == Some(true);
        let plain = |s: &Step, k: u16| s.p1 == k && s.p2 == 0;
        let chara = |s: &Step| s.cond.scene == Some(1) && s.cond.screen == Some(8);
        // ① 一時停止（Esc）→「再開」（一時停止の中で、下を押さずに Z）。
        let esc1 = at(0, &|s| plain(s, keys::ESC));
        let resume = at(esc1 + 1, &|s| paused(s) && plain(s, keys::Z));
        assert_eq!(resume, esc1 + 1, "Esc の次が「再開」でない");
        // ② 一時停止（Esc）→ 下 → Z（タイトルへ戻る）→ 上 → Z（確かめの「はい」）→ 画面 8。
        let esc2 = at(resume + 1, &|s| plain(s, keys::ESC));
        let down = at(esc2 + 1, &|s| paused(s) && plain(s, keys::DOWN));
        for (i, k) in [keys::Z, keys::UP, keys::Z].iter().enumerate() {
            let s = &st[down + 1 + i];
            assert!(
                paused(s) && plain(s, *k),
                "下の次から Z → 上 → Z でない（{i}）"
            );
        }
        let chara2 = at(down + 4, &chara);
        let m2 = at(chara2, &|s| s.note == "match2_start");
        // マッチの終わりの 2 段階の形。`i` の段階が押す段階: その前の段階が「押さずに印を待つ」（menuflag=1・上限が長い）、`i` の段階も
        // menuflag=1 で wait が 30 tick 以上（メニューは出てから 4 フレーム入力を見ない）。会話を送る段階は挟まない。
        let menu_pair = |steps: &[Step], i: usize| {
            assert!(i > 0, "押す段階の前が無い");
            let wait = &steps[i - 1];
            assert_eq!(
                wait.cond.menu_flag,
                Some(true),
                "{}行目: 押す段階の前が menuflag=1 でない",
                steps[i].line
            );
            assert_eq!(wait.cond.scene, Some(2), "{}行目", wait.line);
            assert!(wait.limit >= 18000, "印を待つ上限が短い");
            assert_eq!(wait.p1 | wait.p2, 0, "印を待つ段階が押す");
            assert_eq!(
                steps[i].cond.menu_flag,
                Some(true),
                "{}行目: 押す段階が menuflag=1 でない",
                steps[i].line
            );
            assert!(
                steps[i].wait >= 30,
                "{}行目: 押す前の待ちが 30 tick 未満",
                steps[i].line
            );
        };
        // ③ マッチの終わりのメニューの下は 1 つだけ（2 回目の下で「リプレイを保存して終了」へ届く）。次が Z。
        let ends: Vec<usize> = (m2..st.len())
            .filter(|&i| st[i].p1 & keys::DOWN != 0)
            .collect();
        assert_eq!(ends.len(), 1, "試合 2 の後の下が 1 つでない: {ends:?}");
        let e = ends[0];
        assert_eq!(st[e].note, "end_down");
        assert!(plain(&st[e + 1], keys::Z), "下の次が Z でない");
        menu_pair(st, e);
        assert_eq!(
            st[e + 1].cond.menu_flag,
            Some(true),
            "Z が menuflag=1 でない"
        );
        assert!(st[e + 1].wait >= 30, "Z の前の待ちが 30 tick 未満");
        assert!(
            st[e..].iter().all(|s| s.p1 & (keys::UP | keys::X) == 0),
            "試合の終わりのメニューの後に上・X を押す"
        );
        let chara3 = at(e + 2, &chara);
        let m3 = at(chara3, &|s| s.note == "match3_start");
        // ④ 再戦: 印が立っている間に Z（最初のカーソルの「再戦」）を 3 回。
        let tail = &st[m3..];
        let rematches: Vec<usize> = (0..tail.len())
            .filter(|&i| tail[i].cond.menu_flag == Some(true) && plain(&tail[i], keys::Z))
            .collect();
        assert_eq!(rematches.len(), 3, "再戦の Z が {rematches:?}");
        for &i in &rematches {
            // 次の段階は「前のメニューの印が消えて、次の試合の読み込みが済む」を待つ（同じメニューを 2 回数えない）。
            let next = &tail[i + 1];
            assert_eq!(
                next.cond.menu_flag,
                Some(false),
                "再戦の次が menuflag=0 でない"
            );
            assert_eq!(
                next.cond.loading,
                Some(false),
                "再戦の次が loading=0 でない"
            );
            // 再戦の Z の前も ③ と同じ形（メニューを待つ上限は、マッチが長引いても中止しない長さ。1 マッチは約 12480 tick）。
            menu_pair(tail, i);
        }
        // 会話を送る段階・会話の条件・endmenu は、どの台本にも置かない（会話の間もメニューは Z を受け付ける）。
        for (name, sc) in [("ホスト", &host), ("ゲスト", &guest)] {
            for s in &sc.steps {
                assert!(
                    s.cond.talk.is_none() && !s.optional && s.cond.end_menu.is_none(),
                    "{name}: {}行目に talk / optional / endmenu の段階がある",
                    s.line
                );
            }
        }
        // マッチの中の Z は、`menuflag=1` の段階を除き、マッチの始まり（読み込みの待ち 180 tick を含む）から 63 秒以内の押下だけ
        // （最短でも 2 ラウンドで 100 秒を超えるはずなので、メニューへ届かない。推定）。次のマッチが始まるたび（`menuflag=0` の段階）に数え直す。
        let mut since_start = 0u32;
        for s in tail {
            if s.cond.menu_flag == Some(false) || s.note == "match3_start" {
                since_start = 0;
            }
            since_start += s.wait + s.hold;
            if s.p1 == keys::Z && s.cond.menu_flag != Some(true) {
                assert!(
                    since_start <= 3600 + 180,
                    "{}行目: メニューに届きうる Z（マッチの始まりから {since_start} tick）",
                    s.line
                );
            }
        }
        let g = &guest.steps;
        let g8 = g.iter().filter(|s| chara(s) && s.p2 != 0).count();
        assert_eq!(g8, 3, "ゲストの画面 8 の段階が {g8} 個");
        // ゲストも、ホストと同じ 3 回、印が立つのを条件で待つ（固定の待ちではホストのマッチの長さに追い付けない）。押さない。
        let g_menus: Vec<&Step> = g
            .iter()
            .filter(|s| s.cond.menu_flag == Some(true))
            .collect();
        assert_eq!(
            g_menus.len(),
            3,
            "ゲストの menuflag=1 の段階が {}個",
            g_menus.len()
        );
        assert!(
            g_menus.iter().all(|s| s.p1 | s.p2 == 0 && s.limit >= 18000),
            "ゲストの印を待つ段階が押す・上限が短い"
        );
        let g_last = g.last().unwrap();
        let h_last = host.steps.last().unwrap();
        assert!(
            g_last.cond == Default::default() && g_last.wait > h_last.wait,
            "ゲストの最後の待ちがホストより短い（先に BYE(終了) を出す）"
        );
        let first_match = g.iter().position(|s| s.note == "match_start").unwrap();
        for s in &g[first_match..] {
            if !chara(s) && !s.note.starts_with("char_2p") {
                assert_eq!(s.p1 | s.p2, 0, "ゲストが試合の中で押す（{}行目）", s.line);
            }
        }
    }

    fn match_tail(sc: &higanr_core::script::Script) -> Vec<(u32, u32, u16)> {
        let i = sc
            .steps
            .iter()
            .position(|s| s.note == "match_start")
            .expect("match_start の段階が無い");
        sc.steps[i..]
            .iter()
            .map(|s| (s.wait, s.hold, s.p1 | s.p2))
            .collect()
    }

    /// 2 台の台本の組の共通の決まり（選択の経路と、ホスト 1P・ゲスト 2P の押し分け）。読んだ台本を返す。`allowed_down` は、
    /// タイトルの外で下を押してよい段階の `note`（一時停止のメニューの中 = `pause=1` の段階は名指しなしで通す）。
    fn check_two_game_pair(
        host: &str,
        guest: &str,
        allowed_down: &[&str],
    ) -> (higanr_core::script::Script, higanr_core::script::Script) {
        use higanr_core::input_model::keys;
        use higanr_core::script::{Step, parse};
        let host = parse(host).unwrap();
        let guest = parse(guest).unwrap();
        let screens = |steps: &[Step]| -> Vec<Option<u32>> {
            let mut cur = None;
            steps
                .iter()
                .map(|s| {
                    if s.cond.scene.is_some() {
                        cur = if s.cond.scene == Some(1) {
                            s.cond.screen
                        } else {
                            None
                        };
                    }
                    cur
                })
                .collect()
        };
        assert!(host.steps.iter().all(|s| s.p2 == 0), "ホストが 2P を押す");
        for (s, scr) in guest.steps.iter().zip(screens(&guest.steps)) {
            match scr {
                Some(1 | 6) => assert_eq!(s.p2, 0, "{}行目", s.line),
                Some(7 | 16) => panic!("ゲストが {scr:?} の画面で押す段階がある（{}行目）", s.line),
                _ => assert_eq!(
                    s.p1, 0,
                    "ゲストがネットワーク対戦の中で 1P を押す（{}行目）",
                    s.line
                ),
            }
        }
        for (name, sc) in [("ホスト", &host), ("ゲスト", &guest)] {
            for (s, scr) in sc.steps.iter().zip(screens(&sc.steps)) {
                let allowed = s.cond.pause == Some(true) || allowed_down.contains(&s.note.as_str());
                if (s.p1 | s.p2) & keys::DOWN != 0 && !allowed {
                    assert_eq!(scr, Some(1), "{name}: タイトルの外で下（{}行目）", s.line);
                }
            }
        }
        (host, guest)
    }

    #[test]
    fn the_m2_5_human_ini_is_the_m2_sample_and_can_keep_the_net() {
        let text = include_str!("../../../scripts/m2_05_human.ini");
        let s = from_text(text)
            .unwrap()
            .expect("見本でネットワークが開かない");
        let base = from_text(include_str!("../../../scripts/m2_netplay.ini"))
            .unwrap()
            .unwrap();
        assert_eq!(s, base);
        assert_eq!(s.after_script, AfterScript::Bye);
        let keep = from_text(&text.replace("; after_script = keep", "after_script = keep"))
            .unwrap()
            .unwrap();
        assert_eq!(keep.after_script, AfterScript::Keep);
    }

    #[test]
    fn the_m2_4_host_ini_is_the_m2_sample_plus_a_latency_plan() {
        let s = from_text(include_str!("../../../scripts/m2_04_host.ini"))
            .unwrap()
            .expect("見本でネットワークが開かない");
        let base = from_text(include_str!("../../../scripts/m2_netplay.ini"))
            .unwrap()
            .unwrap();
        assert_eq!(
            NetSettings {
                lat_plan: LatPlan::default(),
                ..s.clone()
            },
            base
        );
        let plan: Vec<(u64, u8)> = (0..s.lat_plan.len())
            .filter_map(|i| s.lat_plan.get(i))
            .collect();
        assert!(plan.len() >= 4, "{plan:?}");
        let mut d = s.latency;
        let (mut up, mut down) = (0, 0);
        for &(_, n) in &plan {
            if n > d {
                up += 1;
            } else {
                down += 1;
            }
            d = n;
        }
        assert!(up > 0 && down > 0, "{plan:?}");
    }

    #[test]
    fn the_dist_ini_and_its_code_side_forms_are_readable() {
        // 取り出しの設定で CRLF になりうるので、行の区切りを揃えてから書き換える。
        let dist = include_str!("../../../scripts/dist/HiganRTool.ini").replace("\r\n", "\n");
        let s = from_text(&dist)
            .unwrap()
            .expect("配る ini でネットワークが開かない");
        assert_eq!(
            (s.scope, s.code_side, s.bind, s.peer),
            (
                Scope::Internet,
                Some(CodeSide::Game),
                Bind::Auto(17723),
                None
            )
        );
        assert_eq!(
            s.clipboard,
            ClipboardKind::System,
            "配る ini は本物のクリップボード"
        );
        // ルームサーバー: Cloudflare のサブドメインを TLS（https → wss・443）で。ルーム名は書かない（2 人が同じ名を書いた見本を
        // 配ると、知らない人どうしが同じルームに入る）。空の行に名を書いた形は読める。
        assert_eq!(
            s.room_server,
            Some(Base {
                secure: true,
                host: "higanr-rooms.higan-re-room.workers.dev".to_string(),
                port: 443
            })
        );
        assert_eq!(s.room, None, "配る ini にルーム名を書かない");
        // 配る ini は観戦を受け付ける（ルームで会ったマッチのホストが配る側になる）。置き場（code_dir）は書かない（この形では断る）。
        assert!(
            s.spectate.as_ref().is_some_and(|sp| sp.code_dir.is_none()),
            "配る ini で観戦を受け付けない: {:?}",
            s.spectate
        );
        assert!(dist.contains("\nroom =\n"), "ルーム名の行（空の形）");
        let named = from_text(&dist.replace("\nroom =\n", "\nroom = 合成の部屋 7\n"))
            .unwrap()
            .expect("ルーム名を書いた形でネットワークが開かない");
        assert_eq!(named.room, Some(RoomText("合成の部屋 7".to_string())));
        assert!(
            !named.text().contains("合成の部屋"),
            "設定の行にルーム名が出た"
        );
        // ディレイは `delay = auto` の 1 行（古いキーは書かない）。前に配った形（`latency = 2`）に戻しても節は断らない。
        assert_eq!(delays(&s), (1, true, 2, true));
        assert!(s.old_keys.is_empty(), "{:?}", s.old_keys);
        assert_eq!(dist.matches("\ndelay = auto\n").count(), 1);
        let old_form = from_text(&dist.replace("\ndelay = auto\n", "\nlatency = 2\n"))
            .unwrap()
            .expect("前に配った形でネットワークが開かない");
        assert_eq!(
            (delays(&old_form), old_form.old_keys),
            ((1, true, 2, true), vec!["latency"])
        );
        assert!(dist.contains("\ncode_side = game\n"), "code_side の行");
        let answer_form = dist.replace("\ncode_side = game\n", "\ncode_side = answer\n");
        let a = from_text(&answer_form)
            .unwrap()
            .expect("answer の形でネットワークが開かない");
        assert_eq!(a.code_side, Some(CodeSide::Answer));
        assert!(
            a.room_server.is_some(),
            "room_server は answer の形でも読む（使わない）"
        );
        let dist = answer_form;
        assert_eq!(
            s.stun,
            Some(StunTarget::Name {
                host: "stun.cloudflare.com".to_string(),
                port: 3478
            })
        );
        // 2 か所目: Google の公開 STUN。1 か所目と別のサーバー。
        assert_eq!(
            s.stun2,
            Some(StunTarget::Name {
                host: "stun.l.google.com".to_string(),
                port: 19302
            }),
            "配る ini の 2 か所目の STUN"
        );
        assert!(dist.contains("\ncode_side = answer\n"), "code_side の行");
        let offer = dist.replace("\ncode_side = answer\n", "\ncode_side = offer\n");
        let o = from_text(&offer)
            .unwrap()
            .expect("offer の形でネットワークが開かない");
        assert_eq!(o.code_side, Some(CodeSide::Offer));
        let with_session = dist.replace(
            "\ncode_side = answer\n",
            "\ncode_side = answer\nsession = 1\n",
        );
        assert!(from_text(&with_session).is_err());
        let replay = higanr_core::autosave::settings_from_ini(&ini::parse(&dist).unwrap())
            .unwrap()
            .expect("配る ini で自動保存が入らない");
        assert_eq!(replay.split, higanr_core::vault::Split::Peer);
    }

    #[test]
    fn the_game_form_takes_a_default_bind_but_the_dev_forms_still_need_one() {
        let dist = include_str!("../../../scripts/dist/HiganRTool.ini").replace("\r\n", "\n");
        let own = dist.replace("bind = auto:17723", "bind = auto:20000");
        assert_eq!(
            from_text(&own).unwrap().unwrap().bind,
            Bind::Auto(20000),
            "書いた値を採る"
        );
        let none = dist.replace("bind = auto:17723\n", "");
        let s = from_text(&none).unwrap().unwrap();
        assert_eq!(
            (s.bind, s.ini_notes.len()),
            (Bind::Auto(DEFAULT_BIND_PORT), 0),
            "書かないのは注にしない"
        );
        for bad in ["auto:0", "auto", "0.0.0.0:1", "x"] {
            let s = from_text(&dist.replace("auto:17723", bad))
                .unwrap()
                .unwrap();
            assert_eq!(s.bind, Bind::Auto(DEFAULT_BIND_PORT), "{bad}");
            assert_eq!(note_keys(&s), vec!["bind"], "{bad}");
        }
        let dev = "[netplay]\nenable = 1\ncode_side = offer\n";
        assert!(from_text(dev).unwrap_err().contains("bind"));
        let e = from_text("[netplay]\nenable = 1\ncode_side = offer\nbind = x\n").unwrap_err();
        assert!(e.contains("bind"), "{e}");
    }

    #[test]
    fn the_shipped_fallbacks_equal_the_shipped_ini() {
        let dist = include_str!("../../../scripts/dist/HiganRTool.ini").replace("\r\n", "\n");
        let ini = ini::parse(&dist).unwrap();
        let get = |k: &str| ini.get(SECTION, k).unwrap();
        assert_eq!(Scope::parse(get("scope")), Some(SHIPPED_SCOPE));
        assert_eq!(get("stun"), SHIPPED_STUN);
        assert_eq!(get("stun2"), SHIPPED_STUN2);
        assert_eq!(get("room_server"), higanr_core::update::UPDATE_SERVER);
        assert_eq!(get("bind"), format!("auto:{DEFAULT_BIND_PORT}"));
    }

    #[test]
    fn unreadable_shipped_lines_fall_back_to_the_shipped_values_with_one_note() {
        let dist = include_str!("../../../scripts/dist/HiganRTool.ini").replace("\r\n", "\n");
        let shipped = from_text(&dist).unwrap().unwrap();
        for (from, to, key) in [
            ("scope = internet", "scope = internnet", "scope"),
            (
                "stun = stun.cloudflare.com:3478",
                "stun = stun.cloudflare.com",
                "stun",
            ),
            ("stun = stun.cloudflare.com:3478", "stun = ::::", "stun"),
            ("stun2 = stun.l.google.com:19302", "stun2 = a b:1", "stun2"),
            (
                "room_server = https://higanr-rooms.higan-re-room.workers.dev",
                "room_server = htp://x",
                "room_server",
            ),
        ] {
            assert!(dist.contains(from), "{from}");
            let s = from_text(&dist.replacen(from, to, 1)).unwrap().unwrap();
            assert_eq!(note_keys(&s), vec![key], "{to}");
            let mut same = s.clone();
            same.ini_notes = vec![];
            assert_eq!(same, shipped, "{to}: 配る値に戻る");
        }
        // 開発の形は書かないときの値（loopback・問い合わせない・ルームサーバー無し）。
        let dev = "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 1\n";
        let s = from_text(&format!("{dev}scope = x\nstun = x\nroom_server = x\n"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (s.scope, s.stun, s.room_server, s.ini_notes.len()),
            (Scope::Loopback, None, None, 3)
        );
    }

    #[test]
    fn the_shipped_ini_has_no_notes_and_each_listed_mistake_gives_one() {
        let dist = include_str!("../../../scripts/dist/HiganRTool.ini").replace("\r\n", "\n");
        assert_eq!(from_text(&dist).unwrap().unwrap().ini_notes, vec![]);
        let edit = |from: &str, to: &str| {
            assert!(dist.contains(from), "{from}");
            dist.replacen(from, to, 1)
        };
        for (text, key) in [
            (edit("upnp = on", "upnp = yes"), "upnp"),
            (edit("; host_seat = 1", "host_seat = 3"), "host_seat"),
            (edit("ipv6 = on", "ipv6 ="), "ipv6"),
            (
                edit("\nenable = 1\n", "\nenable = 1\nunknown_key = 1\n"),
                "unknown_key",
            ),
            (
                dist.replace("[spectate]\n", "[spectate]\nenabel = 1\n"),
                "enabel",
            ),
        ] {
            let s = from_text(&text).unwrap().unwrap_or_else(|| panic!("{key}"));
            assert_eq!(note_keys(&s), vec![key], "{key}");
        }
        let s = from_text(&dist.replace("\n[spectate]\n", "\n[spectate]\n_x = 1\n"))
            .unwrap()
            .unwrap();
        assert!(s.spectate.is_some(), "enable は読める");
    }

    /// 壊れたら落ちる: 配る ini が既定値として書いた行（行を消しても同じになるはずの行）の値が、読み手の既定とずれる。
    /// `host_seat` は「行が無い」ことが「おまかせ」なので、行を書かない（注のまま）。
    #[test]
    fn the_explicit_default_lines_of_the_shipped_ini_equal_leaving_them_out() {
        let dist = include_str!("../../../scripts/dist/HiganRTool.ini").replace("\r\n", "\n");
        let with = from_text(&dist).unwrap().unwrap();
        let mut bare = dist.clone();
        for line in [
            "ipv6 = on\n",
            "room =\n",
            "lan_host =\n",
            "mode = rollback\n",
            "rollback_hit_se = confirmed\n",
        ] {
            assert_eq!(dist.matches(&format!("\n{line}")).count(), 1, "{line}");
            bare = bare.replacen(&format!("\n{line}"), "\n", 1);
        }
        let without = from_text(&bare).unwrap().unwrap();
        assert_eq!(with, without);
        assert!(with.ipv6 && with.room.is_none() && with.lan_host.is_none());
        assert_eq!(with.rollback_hit_se, HitSe::Confirmed);
        assert!(
            !dist.lines().any(|l| l.starts_with("host_seat")),
            "おまかせは行が無いこと"
        );
        // [pacing] の auto_window は処理を軽くする選択肢なので注のまま。外して書けば b-hybrid が効く。
        let pacing = |t: &str| higanr_core::pacing::mode_from_ini(&ini::parse(t).unwrap());
        assert!(pacing(&dist).is_ok());
        let line = "\n; auto_window = b-hybrid\n";
        assert!(dist.contains(line));
        let opened = dist.replacen(line, "\nauto_window = b-hybrid\n", 1);
        assert!(pacing(&opened).is_ok());
        assert_ne!(pacing(&opened), pacing(&dist));
    }

    #[test]
    fn room_server_is_read_as_wss_and_the_room_name_is_never_logged() {
        let base = "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\n";
        let read = |extra: &str| from_text(&format!("{base}{extra}"));
        let none = read("").unwrap().unwrap();
        assert_eq!((&none.room_server, &none.room), (&None, &None));
        assert!(none.text().contains("ルームサーバー 使わない"));
        let b = |secure, host: &str, port| Base {
            secure,
            host: host.to_string(),
            port,
        };
        for (v, want) in [
            (
                "https://rooms.example.dev",
                b(true, "rooms.example.dev", 443),
            ),
            (
                "HTTPS://rooms.example.dev/",
                b(true, "rooms.example.dev", 443),
            ),
            (
                "https://rooms.example.dev:8443",
                b(true, "rooms.example.dev", 8443),
            ),
            ("wss://rooms.example.dev", b(true, "rooms.example.dev", 443)),
            ("http://127.0.0.1:8787", b(false, "127.0.0.1", 8787)),
            ("http://localhost:8787", b(false, "localhost", 8787)),
            ("ws://127.0.0.1:9", b(false, "127.0.0.1", 9)),
        ] {
            let s = read(&format!("room_server = {v}\n")).unwrap().unwrap();
            assert_eq!(s.room_server, Some(want), "{v}");
        }
        for bad in [
            "room_server = http://rooms.example.dev\n",
            "room_server = ws://192.168.1.2:80\n",
            "room_server = rooms.example.dev\n",
            "room_server = https://rooms.example.dev/v0\n",
            "room_server = https://rooms.example.dev:0\n",
            "room = a\n",
        ] {
            let s = read(bad).unwrap().unwrap();
            assert_eq!(s.ini_notes.len(), 1, "{bad:?}");
            let want = if bad.starts_with("room_server") {
                room_server_of(higanr_core::update::UPDATE_SERVER).ok()
            } else {
                None
            };
            assert_eq!((&s.room_server, &s.room), (&want, &None), "{bad:?}");
        }
        let empty = read("room_server =\nroom =\n").unwrap().unwrap();
        assert_eq!((empty.room_server, empty.room), (None, None));
        // ルーム名は文字の決まりの外（`/`）でも読むときには断らない（入力の初めの値にすぎず、決まりの外なら `room_input::initial` が黙って
        // 空へ落とす）。設定の行と {:?} に名は出ない。
        let s = read("room_server = https://rooms.example.dev\nroom = 合成/部屋XYZ\n")
            .unwrap()
            .unwrap();
        assert_eq!(s.room, Some(RoomText("合成/部屋XYZ".to_string())));
        for t in [s.text(), format!("{s:?}")] {
            assert!(!t.contains("部屋XYZ"), "{t}");
        }
        assert!(s.text().contains("初めの値は ini の room"), "{}", s.text());
        let o = from_text(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = offer\nroom_server = https://r.example.dev\n",
        )
        .unwrap()
        .unwrap();
        assert!(o.room_server.is_some());
        assert!(o.text().contains("使うのは code_side = game だけ"));
    }

    #[test]
    fn spectate_is_accepted_with_code_side_game_and_refuses_the_unused_code_dir() {
        let base = "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\nroom_server = ws://127.0.0.1:8787\n";
        let on = from_text(&format!("{base}[spectate]\nenable = 1\n"))
            .unwrap()
            .unwrap();
        assert_eq!(on.code_side, Some(CodeSide::Game));
        assert_eq!(on.spectate, Some(SpectateSettings { code_dir: None }));
        for off in ["", "[spectate]\nenable = 0\n"] {
            let s = from_text(&format!("{base}{off}")).unwrap().unwrap();
            assert_eq!(s.spectate, None, "{off:?}");
        }
        let e = from_text(&format!(
            "{base}[spectate]\nenable = 1\ncode_dir = spec_a\n"
        ))
        .unwrap_err();
        assert!(e.contains("code_dir") && e.contains("game"), "{e}");
        assert!(
            from_text(&format!(
                "{base}[spectate]\nenable = 0\ncode_dir = spec_a\n"
            ))
            .is_ok()
        );
        let v0 = from_text(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = offer\n[spectate]\nenable = 1\ncode_dir = spec_a\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            v0.spectate,
            Some(SpectateSettings {
                code_dir: Some("spec_a".to_string())
            })
        );
    }

    #[test]
    fn code_side_spectate_is_the_viewer_form() {
        let base = "[netplay]\nenable = 1\nscope = lan\nbind = 127.0.0.1:47201\n";
        let s = from_text(&format!(
            "{base}code_side = spectate\nname = viewer\ncode_dir = watch_a\n"
        ))
        .unwrap()
        .unwrap();
        assert_eq!(
            (
                s.code_side,
                s.peer,
                s.session_id,
                s.secret,
                s.code_dir.as_deref()
            ),
            (Some(CodeSide::Spectate), None, None, None, Some("watch_a"))
        );
        assert!(s.peer_addrs().is_empty());
        assert!(s.text().contains("観戦者"), "{}", s.text());
        let copied = format!(
            "{base}code_side = spectate\ndelay = 3\nhost_seat = 2\ncheck = digest\nlat_plan = 10:4\nafter_script = keep\nwait_budget_us = 30000\n"
        );
        let c = from_text(&copied).unwrap().unwrap();
        assert_eq!(c.code_side, Some(CodeSide::Spectate));
        assert_eq!(c.wait_budget_us, 30_000);
        for bad in [
            "peer = 127.0.0.1:7002\n",
            "session = 1\n",
            "secret = 000102030405060708090a0b0c0d0e0f\n",
            "stun = 127.0.0.1:3478\n",
            "stun2 = 127.0.0.1:3479\n",
            "candidates = mapped\n",
            "code_dir =\n",
        ] {
            let e = from_text(&format!("{base}code_side = spectate\n{bad}"));
            assert!(e.is_err(), "{bad:?} を通した");
        }
        let e =
            from_text(&format!("{base}code_side = spectate\npeer = 127.0.0.1:1\n")).unwrap_err();
        assert!(e.contains("peer") && e.contains("spectate"), "{e}");
        let e = from_text(&format!(
            "{base}code_side = spectate\n[spectate]\nenable = 1\n"
        ))
        .unwrap_err();
        assert!(e.contains("観戦のホスト"), "{e}");
        assert!(
            from_text(&format!(
                "{base}code_side = spectate\n[spectate]\nenable = 0\n"
            ))
            .unwrap()
            .is_some()
        );
        assert_eq!(
            from_text(&format!("{base}code_side = offer\n"))
                .unwrap()
                .unwrap()
                .code_side,
            Some(CodeSide::Offer)
        );
        assert!(from_text(&format!("{base}code_side = Spectate\n")).is_err());
    }

    #[test]
    fn the_spectate_section_only_adds_a_host_and_is_strict() {
        let base = from_text(OK).unwrap().unwrap();
        assert_eq!(base.spectate, None);
        for off in ["", "[spectate]\n", "[spectate]\nenable = 0\n"] {
            let s = from_text(&format!("{OK}{off}")).unwrap().unwrap();
            assert_eq!(s, base, "{off:?}: 観戦を使わない書き方で設定が変わった");
        }
        let on = from_text(&format!("{OK}[spectate]\nenable = 1\n"))
            .unwrap()
            .unwrap();
        assert_eq!(on.spectate, Some(SpectateSettings { code_dir: None }));
        assert_eq!(
            NetSettings {
                spectate: None,
                ..on.clone()
            },
            base
        );
        let dir = from_text(&format!("{OK}[spectate]\nenable = 1\ncode_dir = spec_a\n"))
            .unwrap()
            .unwrap();
        assert_eq!(
            dir.spectate,
            Some(SpectateSettings {
                code_dir: Some("spec_a".to_string())
            })
        );
        // 知らないキーと読めない enable は、その行だけ捨てる。code_dir の空は従来どおり断る。
        let typo = from_text(&format!("{OK}[spectate]\nenable = 1\nenabel = 1\n"))
            .unwrap()
            .unwrap();
        assert!(typo.spectate.is_some());
        assert_eq!(note_keys(&typo), vec!["enabel"]);
        let word = from_text(&format!("{OK}[spectate]\nenable = yes\n"))
            .unwrap()
            .unwrap();
        assert!(word.spectate.is_none());
        assert_eq!(note_keys(&word), vec!["enable"]);
        assert_eq!(
            (word.ini_notes[0].line, word.ini_notes[0].section.as_str()),
            (7, "spectate")
        );
        assert!(from_text(&format!("{OK}[spectate]\nenable = 1\ncode_dir =\n")).is_err());
        let off = "[netplay]\nenable = 0\nbind = 127.0.0.1:0\n[spectate]\nenable = 1\n";
        assert_eq!(from_text(off).unwrap(), None);
    }

    #[test]
    fn the_name_from_ini_keeps_only_allowed_characters_and_falls_back_to_the_default() {
        assert_eq!(player_name_of(Some("Alice-2_b")), "Alice-2_b");
        assert_eq!(player_name_of(Some("A l.i@c/e")), "Ali@ce", "@ は残る");
        assert_eq!(player_name_of(Some("  Bob  ")), "Bob", "空白は落ちる");
        assert_eq!(player_name_of(Some("テスト太郎x")), "x");
        assert_eq!(DEFAULT_NAME, "HiganRTool");
        for all_out in [Some(""), Some("   "), Some("テスト"), Some(". !#"), None] {
            assert_eq!(player_name_of(all_out), DEFAULT_NAME, "{all_out:?}");
        }
        assert_eq!(player_name_of(Some(&"a".repeat(33))), "a".repeat(32));
        assert_eq!(player_name_of(Some(&"a".repeat(32))), "a".repeat(32));
        assert_eq!(player_name_of(Some(&"a ".repeat(40))), "a".repeat(32));
    }
}
