//! 起動の引数。外部のクレートを入れず、手で読む（引数は十数個で、形も単純なため）。

use std::net::SocketAddr;
use std::path::PathBuf;

use std::time::{SystemTime, UNIX_EPOCH};

use higanr_core::auth::Secret;
use higanr_core::candidate::Candidates;
use higanr_core::code;
use higanr_core::frame::MAX_DELAY;
use higanr_core::handshake::{ModeWish, Name};
use higanr_core::rollback;
use higanr_core::seal::{Ephemeral, KeySeed};
use higanr_core::wire::{Mode, Seat};

use crate::fault::{self, FaultConfig};
use crate::shape::{self, Dir};
use crate::sim::{self, LatStep, SimConfig};
use crate::stun_fake;
use crate::stun_probe;
use crate::watch::WatchConfig;
use higanr_core::guard::{self, Scope};
use higanr_core::pacing::DEFAULT_FPS;

pub const USAGE: &str = "\
peer_sim —— 偽の対戦相手（HiganRTool のプロトコルだけを話す。既定は loopback だけ。--scope で広げる）

使い方:
  peer_sim --bind 127.0.0.1:7001 --peer 127.0.0.1:7002 [選択肢]

必須:
  --bind ADDR        自分の受信ソケット（ネットワークの範囲の中の IP:ポート。既定の範囲では 127.0.0.0/8 か [::1]。
                     名前は受けない。全部のアドレス 0.0.0.0 / [::] は受けない）
  --peer ADDR        相手の受信ソケット（ネットワークの範囲の中）。何度でも書ける（相手の候補の列。全部へ HELLO を送り、
                     最初に返しが来た経路を採る。--code を使うときは書かない）

ネットワークの範囲:
  --scope S          loopback | lan | internet（既定 loopback）。lan は loopback ＋ 私設のアドレス（10/8・
                     172.16/12・192.168/16・169.254/16・fc00::/7・fe80::/10）、internet はそれに外のアドレスを足す。
                     --bind・--peer・--code の候補・--stun の全部がこの範囲の中でなければ断る。LAN・外の相手へ
                     届くには --bind にも自分のそのポートのアドレスを書く

接続:
  --name S           名前（既定 peer_sim。32 B まで）
  --session N        session_id（両者で同じ値。既定 0x48475254）
  --secret HEX       接続の秘密（16 進 32 桁。両者で同じ値。既定 706565725f73696d2064656661756c74 =
                     \"peer_sim default\"。dll の ini の secret と揃える）
  --code CODE        相手（始める側）の接続コード。session・秘密・相手の候補をここから取る
                     （--peer・--session・--secret と一緒に使わない。候補はネットワークの範囲の中だけ）
  --stun ADDR        STUN の問い合わせ先（ネットワークの範囲の中だけ。答えは記録に出すだけで、接続は待たない）
  --nonce N          role_nonce（大きい方がホスト。既定は時刻とプロセス番号から）
  --latency N        希望 latency 0〜9（既定 2。両者の大きい方になる）
  --host-seat 1|2    ホストになったときの席（既定 1）
  --wait-sec N       接続の手順が時間切れ（10 秒）になっても、起動から N 秒（1〜3600）までは作り直して
                     待ち続ける（相手の花映塚の起動が遅い回。書かなければ 10 秒でやめる。--max-sec より
                     長くは書けない。--max-sec を省くと、その既定に N 秒を足す）

試合:
  --seed N           自分の語の列の seed（既定 1）
  --segs N           seg の数（既定 2）
  --frames N         seg ごとのフレーム数（既定 600）
  --load-ms N        seg に入る前の読み込みの長さ ms（既定 0）
  --follow           相手（dll）の試合の長さに付いていく（--segs・--frames と一緒に使わない。
                     相手の BYE(終了) か 60 秒の沈黙まで続ける）
  --mask N           送る語に掛けるマスク（既定 0xFFFF。本物の花映塚を相手にするときは 0xC4 =
                     Shift・左右だけ。Z・X・Esc・上下はメニューのカーソルを動かす・決める）
  --life N           自分の SELECT の初期ライフ 1〜10（既定 10。dll が相手の席へ書く）
  --lat S:F:N        ホストのとき、seg S のフレーム F の取得の前に delay を N に（何度でも。
                     --follow の seg の番号は相手と同じ数え方 —— 試合・再戦ごとに 1 つ進む）
  --fps N            1 秒のフレーム数 1〜1000（既定 60。試合と選択の 1 フレームの枠と、台本の wait の単位）。
                     dll の [pacing] mode = b / fps = N で速く回す回に揃える。--follow の試合は dll に付いていく
  --mode delay|rollback  望むモード（既定 delay）。両者が rollback を望んだときだけ rollback で試合をする
                     （相手の語が届いていないフレームを予測で進め、届いた語で進め直す）。rollback は
                     --lat と一緒に使わない（--follow とは使える。付いていく側は予測せず、相手が取った
                     フレームまでを使う）
  --rb-delay N       rollback に決まったときの D 0〜9（既定 1。ホストの値が使われる。--mode rollback と一緒に）
  --rb-ahead N       rollback に決まったときの R（予測の上限）1〜16（既定 8。通信に載せない。--mode rollback と一緒に）
  --rb-sync on|off   rollback の時刻合わせ（先にいる側が描画だけのフレームで少しずつ待って揃える。既定 on。通信に載せない。
                     off は時刻合わせなしの実行と比べるため。--mode rollback と一緒に）

選択:
  --menu FILE        選択の台本（決めた MENU の語と画面を移る押下を、画面の模型を追いながら出す。
                     形は src/peer_sim/README.md。無ければ v0: ホストが相手を見たら「決定」を 1 回）

わざと壊す（送る側だけ）:
  --drop P  --delay P  --dup P   落とす・遅らせる・重ねる割合（%）
  --delay-max MS                 遅らせる幅（1〜MS ms。パケットごとに引くので、4 ms ごとの送り直しの
                                 一番早いコピーで遅れが打ち消される）
  --one-way-ms N                 全部の送りを N ms 遅らせる（0〜1000。送り直しも同じだけ遅れるので、
                                 相手から見た片道が N ms 長くなる。RTT を模す。上の 3 つと一緒に使える）
  --jitter-ms N                  コピー 1 通ごとに 0〜N ms の乱数の遅れを足す（0〜1000。送り直しにも別々に掛かるので
                                 打ち消されず、届く順が入れ替わる。実回線のジッタを模す。既定 0）
  --jitter-shape uniform|spiky   揺らぎの分布（既定 uniform。spiky は 9 割が 0〜N/10、1 割が N/2〜N）
  --fault-seed N                 乱数の seed（既定 1）
  --drop-every N  --drop-run K   決め打ち: N の倍数の通番から K 通続けて落とす
  --drop-after N                 決め打ち: N 通を越えたら全部落とす（繋がった後に黙る相手）
  --proto-ver N                  通信の上でバージョン N の HiganRTool を名乗る（0〜255。今のバージョンと違う数。送るものと受けるものの
                                 両方でバージョンを入れ替える。バージョン違いで断る経路を loopback で確かめる）

観戦（M6-1）:
  --spectate-host ADDR  対戦のホストの役のとき、観戦者を受けるアドレスを ADDR（ネットワークの範囲の中の IP:ポート。ポート 0 は不可。
                     --bind と別）に開き、使うと決まった記録（SELECT・選択の押下・MENU の語・試合の語・終わり）を
                     観戦者へ流す。起動時に観戦用の接続コードを 1 行出す。ゲストになった走行では流さない
  --spectate-session N  観戦用の session_id（既定 0x53504543）。対戦の session と同じ値は断る
  --spectate-secret HEX 観戦用の秘密（16 進 32 桁。既定 706565725f73696d2077617463686572 =
                     \"peer_sim watcher\"）。対戦の秘密と同じ値は断る
  --spectate-fixed-key  観戦の配る側の鍵の種を、観戦用の秘密と session から決める（既定は走るたびに OS 起源の乱数）。
                     観戦用のコードを使わない `--watch --peer`（ホストの公開鍵を式で作る経路）の検査のためだけ。
                     同じ秘密・session で起こし直すと同じ鍵で通し番号が 0 から重なるので、起こし直す実行には使わない
  --watch            観戦者の役で走る（走らせ方が別。下の「観戦者」）

観戦者（--watch。ホストの観戦の受信ソケットへ SPECTATE_JOIN を送り、流れを CSV に書く）:
  peer_sim --watch --bind ADDR (--code 観戦用のコード | --peer ホストの観戦の受信ソケット [--session N] [--secret HEX])
           [--name S] [--nonce N] [--scope S] [--start-after-ms N] [--silent-sec N] [--max-sec N] [--log-dir DIR]
  --code             観戦用の接続コード（ホストが出す。対戦用のコードは断る）。--peer・--session・--secret と一緒に使わない
  --start-after-ms N 走り始めてから最初の JOIN までの長さ（既定 0。遅れて入る回）
  --silent-sec N     流れを聞いた後、これだけ何も聞かなければ「ホストが黙った」で終える（既定 10）
  --max-sec N        打ち切り（既定 1800）
  終わったら、試合の語 seg,frame,p1,p2 の CSV と、SELECT・押下・MENU・終わりを記録の番号つきで並べた CSV を --log-dir へ
  回ごとに別名で書く。終了コード 0 = ホストの終わり（BYE(終了)）まで受けた / 1 = それ以外 / 2 = 引数・ソケットの失敗

突き合わせ（走らせない。M2-4）:
  --compare DLL_CSV PEER_LOG   dll の _net_frames.csv.gz（平文の .csv も読む）と peer_sim の記録の列が同じかを見て、dll の余裕と待ちの
                               分布・delay の変わり目を出す（一致なら終了コード 0、食い違いなら 1。どちらかが
                               0 行なら比べられていないので 1）
  --compare2 A_CSV B_CSV       2 台の本物の花映塚の dll の _net_frames.csv.gz（平文の .csv も読む）どうしの列が同じかを見て、両者の余裕と
                               待ちの分布・待ちの 1 ms の枠を出す（一致なら終了コード 0、食い違いなら 1。どちらかが
                               0 行なら比べられていないので 1）
  --compare-spectate WATCH_CSV PLAYER [EVENTS_CSV]
                               観戦者の試合の語の CSV（seg,frame,p1,p2）が、対戦者の記録 PLAYER（dll の _net_frames.csv.gz か
                               peer_sim の記録）と同じかを見る。遅れて入った観戦者は、対戦者の列の途中の seg の始まりから
                               始まっていてよい（seg の途中からは不可）。観戦者の後に残った対戦者の行は 1 行まで許す。
                               EVENTS_CSV（観戦者の記録の CSV）と peer_sim の記録があれば、押下の並びも比べる
                               （一致なら終了コード 0、食い違いなら 1。観戦者が 0 行なら比べられていないので 1）

偽の STUN を立てる（走らせない。開発用。LAN の回。外の STUN サーバーへは何も送らない）:
  --stun-fake ADDR   偽の STUN の応答役を ADDR（IP:ポート。既定の範囲では loopback）に立て、Binding Request に送り元の
                     アドレスを返す。答える相手の範囲は --scope（既定 loopback。--scope lan で私設のアドレスの送り元にも
                     答える）。--scope と --max-sec だけと一緒に書ける。--max-sec（1〜7200。既定 1800）で自分で終わる。
                     LAN の相手の dll は LAN のアドレスから送るので、127.0.0.1 に立てて --scope lan を書けば足りる
                     （LAN のアドレスに受信ソケットを開くとファイアウォールの確かめが出うる）

観戦用のコードの候補を書き換える（走らせない。開発用。観戦者の dll を --shape の中継越しに繋ぐ。README の「観戦用のコードの書き換え」）:
  --rewrite-spectate-code IN OUT --candidate ADDR [--scope S] [--overwrite]
                     ホストの dll が書いた観戦用のコード（IN。spectate_code.txt の中身）の候補を ADDR 1 つに置き換え、検算を
                     付け直して OUT に書く（session・秘密・名前・期限・種別はそのまま。秘密は出力に出さない）。ADDR は中継の
                     「観戦者向けの受信ソケット」（--shape --a ホストの受信ソケット --b 観戦者の受信ソケット なら --via-b）。対戦用のコード・壊れた
                     コード・期限切れは断る。ADDR はネットワークの範囲（--scope。既定 loopback）の中だけ。OUT が在れば --overwrite が要る。
                     OUT はゲームのフォルダの中には書かない

遅延とロスを揺らす中継（走らせない。開発用。本物の花映塚 2 台の間に挟む。loopback だけ。README の「--shape」）:
  --shape --a ADDR --b ADDR --via-a ADDR --via-b ADDR [乱れ] [--max-sec N]
                     中継は受信ソケットを 2 つ開く。--via-a（A 向けの受信ソケット。A の ini の peer に書く）で A（--a = A の ini の bind）から
                     受けたものを --via-b から B へ、--via-b で B（--b）から受けたものを --via-a から A へ、中身を
                     読まずに運ぶ。送り元が違うものは捨てて数える。アドレスは 4 つとも別々の loopback（127.0.0.0/8 か [::1]。
                     --scope は受けない）。--max-sec（1〜7200。既定 1800）で自分で終わり、10 秒ごとと終わりに、向きごとの
                     通数・落とした・遅らせた・重ねた・捨てた（送り元違い）を 1 行出す
  乱れ               上の「わざと壊す」の --drop --delay --dup --delay-max --one-way-ms --fault-seed --drop-every
                     --drop-run --drop-after を、そのまま書くと両方向に、--a2b- / --b2a- を先頭に付けると（例 --a2b-drop 5）
                     その向きだけに掛ける。同じ項目は向き指定が勝つ。乱数は向きごとに別の列（--fault-seed を書けば
                     A→B はその値、B→A はそれを混ぜた値。--b2a-fault-seed で別に決められる）

ルーターの見え方を測る（走らせない。開発用）:
  --stun-probe A B   1 つの受信ソケットから STUN の 2 か所（A と B。名前:ポート か IP:ポート。別々のサーバー）へ Binding を
                     問い合わせ、見えたアドレス 2 行と「ポートが同じ（宛先に依らない割り当て）/ 違う（宛先ごとに変わる
                     割り当て）」の 1 行を出して終わる。外の STUN サーバーへ問い合わせるので、**--scope internet と
                     --bind（自分の LAN のアドレス。ポート 0 でも可）だけと一緒に書く**（ほかの引数は受けない。
                     外へ出る通信なので、この道具は loopback だけで使う）

記録:
  --log-dir DIR     記録の置き場（既定は exe と同じフォルダの logs\\）。回ごとに別名で、上書きしない
  --max-sec N        これを過ぎたらやめる秒数（既定はフレーム数から。--follow では 1800。--wait-sec を
                     書けばその秒数を足す）。繋がった後に打ち切るときは、相手へ BYE(終了) を 1 通出してから終える
  --help
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub bind: SocketAddr,
    pub sim: SimConfig,
    /// 選択の台本のファイル（`--menu`。読むのは exe の `main.rs`）。
    pub menu: Option<PathBuf>,
    pub log_dir: Option<PathBuf>,
    /// 1 秒のフレーム数（`--fps`）。
    pub fps: u64,
    /// 接続の手順を作り直して待ち続ける長さ（ms。`--wait-sec` × 1000）。None なら `Link::set_retry_window` を呼ばない
    /// （10 秒の時間切れでやめる）。
    pub wait_ms: Option<u32>,
    pub scope: Scope,
    /// 観戦のポート（`--spectate-host`。読んで開くのは exe の `main.rs`）。
    pub spectate: Option<SpectateCli>,
    /// 望むモードと D（`--mode` / `--rb-delay`）。
    pub mode: ModeWish,
    /// rollback に決まったときの R（`--rb-ahead`）。
    pub ahead: u8,
    /// rollback に決まったときの時刻合わせ（`--rb-sync on|off`）。
    pub time_sync: bool,
}

/// `--spectate-host` の値（観戦者を受けるポートと、観戦用のコードの session・秘密・配る側の鍵の種）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpectateCli {
    pub bind: SocketAddr,
    pub session_id: u32,
    pub secret: Secret,
    /// 配る側の使い捨ての鍵の種。`parse` は走るたびの新しい種を入れる（`--spectate-fixed-key` のときだけ
    /// `keys::spectate_host_seed`）。観戦用のコードの公開鍵はこの種の公開鍵。
    pub key_seed: KeySeed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchCli {
    pub bind: SocketAddr,
    pub cfg: WatchConfig,
    pub scope: Scope,
    pub log_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteCli {
    /// 元の観戦用のコードのファイル（読むだけ）。
    pub input: PathBuf,
    pub output: PathBuf,
    pub candidate: SocketAddr,
    pub scope: Scope,
    /// 出力先が既にあっても書く。
    pub overwrite: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Run(Box<Cli>),
    /// `--rewrite-spectate-code IN OUT --candidate ADDR`（観戦用のコードの候補の書き換え）。
    RewriteSpectateCode(Box<RewriteCli>),
    /// `--watch ...`（観戦者の役）。
    Watch(Box<WatchCli>),
    /// `--compare-spectate WATCH_CSV PLAYER [EVENTS_CSV]`（観戦者の列と対戦者の列の突き合わせ）。
    CompareSpectate(PathBuf, PathBuf, Option<PathBuf>),
    Help,
    /// `--compare DLL_CSV PEER_LOG`（走らせずに 2 つの記録を突き合わせる）。
    Compare(PathBuf, PathBuf),
    /// `--compare2 A_CSV B_CSV`（2 台の dll の記録どうしを突き合わせる）。
    Compare2(PathBuf, PathBuf),
    /// `--stun-probe A B`（2 か所の STUN へ 1 つの受信ソケットから問い合わせて見え方を比べる）。
    StunProbe(Box<stun_probe::Args>),
    /// `--stun-fake ADDR`（偽の STUN の応答役を起こして待つ。LAN の回）。
    StunFake(Box<stun_fake::Args>),
    /// `--shape ...`（遅延とロスを揺らす UDP の中継）。
    Shape(Box<shape::Args>),
}

/// 付いていく形の既定の打ち切り（ms。30 分）。
pub const FOLLOW_MAX_MS: u64 = 30 * 60 * 1_000;

/// `--wait-sec` の上限（秒。1 時間）。dll が待つ 30 分（`netplay::HANDSHAKE_RETRY_MS` = 接続コードの期限）より長く取れるので、
/// dll の側を何度起こし直しても peer_sim の側が先に諦めずに済む。下限 1 は、0 を「待たない」と読ませず断るため。
pub const MAX_WAIT_SEC: u32 = 3_600;

/// 既定の session_id（`"HGRT"` のリトルエンディアン。両者が引数を省いたときに同じ値になればよい）。
pub const DEFAULT_SESSION: u32 = 0x4847_5254;

/// 既定の秘密（`"peer_sim default"` の 16 B。両者が引数を省いたときに同じ値になればよい。秘密として守る値ではない —— 本物の秘密は
/// 接続を始める側の dll が OS の乱数で引く）。
pub const DEFAULT_SECRET: Secret = Secret(*b"peer_sim default");

/// 観戦用のコードの既定の session_id（`"SPEC"` の ASCII。対戦の `DEFAULT_SESSION` と別の値にする。同じ値だと観戦者が対戦の鍵を
/// 作れてしまう）。検査の既定で、本物の値は dll が引く。
pub const DEFAULT_SPECTATE_SESSION: u32 = 0x5350_4543;

/// 観戦用のコードの既定の秘密（`"peer_sim watcher"` の 16 B。`DEFAULT_SECRET` と別の値。守る値ではない）。
pub const DEFAULT_SPECTATE_SECRET: Secret = Secret(*b"peer_sim watcher");

/// 観戦者の既定の「ホストが黙った」の長さ（ms）。ホストの合図の間隔（`spectate::HEARTBEAT_MS` = 1 秒）の 10 回ぶん
/// （ホストが観戦者を外す `SILENT_MS` と同じ）。
pub const DEFAULT_SILENT_END_MS: u32 = 10_000;

pub const DEFAULT_WATCH_MAX_MS: u32 = 30 * 60 * 1_000;

fn num(s: &str, what: &str) -> Result<u64, String> {
    let r = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16),
        None => s.parse::<u64>(),
    };
    r.map_err(|_| format!("{what} の値 {s:?} が数として読めない"))
}

fn num_in<T: TryFrom<u64>>(s: &str, what: &str) -> Result<T, String> {
    let v = num(s, what)?;
    T::try_from(v).map_err(|_| format!("{what} の値 {v} が大きすぎる"))
}

fn addr(s: &str, what: &str) -> Result<SocketAddr, String> {
    s.parse::<SocketAddr>().map_err(|_| {
        format!("{what} の値 {s:?} が IP:ポート として読めない（名前は受けない。127.0.0.1:7001 や [::1]:7001 の形で）")
    })
}

fn pct(s: &str, what: &str) -> Result<u8, String> {
    let v: u8 = num_in(s, what)?;
    if v > 100 {
        return Err(format!("{what} は 0〜100（%）"));
    }
    Ok(v)
}

fn lat_step(s: &str) -> Result<LatStep, String> {
    let parts: Vec<&str> = s.split(':').collect();
    let [seg, frame, delay] = parts.as_slice() else {
        return Err(format!("--lat の値 {s:?} は S:F:N の形で"));
    };
    let delay: u8 = num_in(delay, "--lat の N")?;
    if delay > MAX_DELAY {
        return Err(format!("--lat の N は 0〜{MAX_DELAY}"));
    }
    Ok(LatStep {
        seg: num_in(seg, "--lat の S")?,
        frame: num_in(frame, "--lat の F")?,
        delay,
    })
}

/// 引数（プログラム名を除く）を読む。宛先と受信ソケットがネットワークの範囲（`--scope`）の外なら、ここで断る。`--code` の有効期限は
/// 今の時刻（UNIX 秒）で見る。
pub fn parse(args: &[String], default_nonce: u64) -> Result<Parsed, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    parse_at(args, default_nonce, now)
}

pub fn parse_at(args: &[String], default_nonce: u64, now_unix: u64) -> Result<Parsed, String> {
    let mut bind = None;
    let mut peers: Vec<SocketAddr> = Vec::new();
    let mut name = "peer_sim".to_string();
    let mut session_id = DEFAULT_SESSION;
    let mut secret = DEFAULT_SECRET;
    let mut code: Option<String> = None;
    let mut stun = None;
    // --code と一緒に使えない引数を見たか。
    let mut explicit_link = false;
    let mut role_nonce = default_nonce;
    let mut latency: u8 = 2;
    let mut host_seat = Seat::P1;
    let mut seed: u64 = 1;
    let mut follow = false;
    let mut word_mask: u16 = 0xFFFF;
    let mut life: u8 = 10;
    let mut segs: Option<u32> = None;
    let mut frames: Option<u32> = None;
    let mut load_ms: u32 = 0;
    let mut lat_script = Vec::new();
    let mut fault = FaultConfig {
        seed: 1,
        ..FaultConfig::default()
    };
    let mut log_dir = None;
    let mut menu = None;
    let mut max_sec: Option<u32> = None;
    let mut wait_sec: Option<u32> = None;
    let mut fps = DEFAULT_FPS;
    let mut scope: Option<Scope> = None;
    let mut spectate_host: Option<SocketAddr> = None;
    let mut spectate_session: Option<u32> = None;
    let mut spectate_secret: Option<Secret> = None;
    let mut spectate_fixed_key = false;
    let mut mode = Mode::Delay;
    let mut rb_delay: Option<u8> = None;
    let mut rb_ahead: Option<u8> = None;
    let mut rb_sync: Option<bool> = None;

    if args.first().map(String::as_str) == Some("--compare-spectate") {
        return match args {
            [_, watch, player] => Ok(Parsed::CompareSpectate(
                PathBuf::from(watch),
                PathBuf::from(player),
                None,
            )),
            [_, watch, player, events] => Ok(Parsed::CompareSpectate(
                PathBuf::from(watch),
                PathBuf::from(player),
                Some(PathBuf::from(events)),
            )),
            _ => Err(
                "--compare-spectate は観戦者の試合の語の CSV と対戦者の記録の 2 つ（と、あれば観戦者の記録の CSV）を取る"
                    .to_string(),
            ),
        };
    }
    if args.first().map(String::as_str) == Some("--rewrite-spectate-code") {
        return parse_rewrite(args);
    }
    if args.iter().any(|a| a == "--watch") {
        return parse_watch(args, default_nonce, now_unix);
    }
    if args.first().map(String::as_str) == Some("--compare") {
        return match args {
            [_, dll, peer] => Ok(Parsed::Compare(PathBuf::from(dll), PathBuf::from(peer))),
            _ => Err(
                "--compare は dll の _net_frames.csv と peer_sim の記録の 2 つだけを取る"
                    .to_string(),
            ),
        };
    }
    if args.first().map(String::as_str) == Some("--compare2") {
        return match args {
            [_, a, b] => Ok(Parsed::Compare2(PathBuf::from(a), PathBuf::from(b))),
            _ => Err("--compare2 は 2 つの dll の _net_frames.csv だけを取る".to_string()),
        };
    }
    if args.iter().any(|a| a == "--stun-probe") {
        return parse_stun_probe(args);
    }
    if args.iter().any(|a| a == "--stun-fake") {
        return parse_stun_fake(args);
    }
    if args.iter().any(|a| a == "--shape") {
        return parse_shape(args);
    }
    let mut it = args.iter();
    while let Some(k) = it.next() {
        if k == "--help" || k == "-h" {
            return Ok(Parsed::Help);
        }
        if k == "--follow" {
            follow = true;
            continue;
        }
        if k == "--spectate-fixed-key" {
            spectate_fixed_key = true;
            continue;
        }
        let v = it
            .next()
            .ok_or_else(|| format!("{k} の後に値が要る"))?
            .as_str();
        match k.as_str() {
            "--bind" => bind = Some(addr(v, k)?),
            "--peer" => {
                peers.push(addr(v, k)?);
                explicit_link = true;
            }
            "--name" => name = v.to_string(),
            "--session" => {
                session_id = num_in(v, k)?;
                explicit_link = true;
            }
            "--secret" => {
                secret = Secret::from_hex(v)?;
                explicit_link = true;
            }
            "--code" => {
                if code.is_some() {
                    return Err("--code は 1 回だけ".to_string());
                }
                code = Some(v.to_string());
            }
            "--stun" => stun = Some(addr(v, k)?),
            "--scope" => {
                if scope.is_some() {
                    return Err("--scope は 1 回だけ".to_string());
                }
                scope = Some(
                    Scope::parse(v)
                        .ok_or_else(|| format!("--scope の値 {v:?} は {}", Scope::NAMES))?,
                );
            }
            "--spectate-host" => {
                if spectate_host.replace(addr(v, k)?).is_some() {
                    return Err("--spectate-host は 1 回だけ".to_string());
                }
            }
            "--spectate-session" => spectate_session = Some(num_in(v, k)?),
            "--spectate-secret" => spectate_secret = Some(Secret::from_hex(v)?),
            "--nonce" => role_nonce = num(v, k)?,
            "--latency" => {
                latency = num_in(v, k)?;
                if latency > MAX_DELAY {
                    return Err(format!("--latency は 0〜{MAX_DELAY}"));
                }
            }
            "--host-seat" => {
                host_seat = match v {
                    "1" => Seat::P1,
                    "2" => Seat::P2,
                    _ => return Err("--host-seat は 1 か 2".to_string()),
                }
            }
            "--mode" => {
                mode = match v {
                    "delay" => Mode::Delay,
                    "rollback" => Mode::Rollback,
                    _ => return Err("--mode は delay か rollback".to_string()),
                }
            }
            "--rb-delay" => {
                let d: u8 = num_in(v, k)?;
                if d > MAX_DELAY {
                    return Err(format!("--rb-delay は 0〜{MAX_DELAY}"));
                }
                rb_delay = Some(d);
            }
            "--rb-ahead" => {
                let r: u8 = num_in(v, k)?;
                if !(rollback::MIN_AHEAD..=rollback::MAX_AHEAD).contains(&r) {
                    return Err(format!(
                        "--rb-ahead は {}〜{}",
                        rollback::MIN_AHEAD,
                        rollback::MAX_AHEAD
                    ));
                }
                rb_ahead = Some(r);
            }
            "--rb-sync" => {
                rb_sync = Some(match v {
                    "on" => true,
                    "off" => false,
                    _ => return Err("--rb-sync は on か off".to_string()),
                })
            }
            "--seed" => seed = num(v, k)?,
            "--segs" => segs = Some(num_in(v, k)?),
            "--frames" => frames = Some(num_in(v, k)?),
            "--load-ms" => load_ms = num_in(v, k)?,
            "--mask" => word_mask = num_in(v, k)?,
            "--life" => {
                life = num_in(v, k)?;
                if !(1..=10).contains(&life) {
                    return Err("--life は 1〜10".to_string());
                }
            }
            "--lat" => lat_script.push(lat_step(v)?),
            "--drop" => fault.drop_pct = pct(v, k)?,
            "--delay" => fault.delay_pct = pct(v, k)?,
            "--dup" => fault.dup_pct = pct(v, k)?,
            "--delay-max" => fault.delay_max_ms = num_in(v, k)?,
            // 範囲は `fault.validate()` が見る（上限 `fault::MAX_ONE_WAY_MS`）。
            "--one-way-ms" => fault.one_way_ms = num_in(v, k)?,
            // 範囲は `fault.validate()` が見る（上限 `fault::MAX_JITTER_MS`）。
            "--jitter-ms" => fault.jitter_ms = num_in(v, k)?,
            "--jitter-shape" => {
                fault.jitter_shape = fault::JitterShape::parse(v).ok_or_else(|| {
                    format!("--jitter-shape の値 {v:?} は {}", fault::JitterShape::NAMES)
                })?
            }
            "--fps" => {
                fps = num(v, k)?;
                sim::check_fps(fps).map_err(|e| format!("--fps: {e}"))?;
            }
            "--fault-seed" => fault.seed = num(v, k)?,
            // 今のバージョンと同じ数は `fault.validate()` が断る。
            "--proto-ver" => fault.claim_ver = Some(num_in(v, k)?),
            "--drop-every" => fault.drop_every = num_in(v, k)?,
            "--drop-run" => fault.drop_run = num_in(v, k)?,
            "--drop-after" => fault.drop_after = num(v, k)?,
            "--log-dir" => log_dir = Some(PathBuf::from(v)),
            "--menu" => {
                if menu.is_some() {
                    return Err("--menu は 1 回だけ".to_string());
                }
                menu = Some(PathBuf::from(v));
            }
            "--max-sec" => max_sec = Some(num_in(v, k)?),
            "--wait-sec" => {
                let w: u32 = num_in(v, k)?;
                if !(1..=MAX_WAIT_SEC).contains(&w) {
                    return Err(format!("--wait-sec は 1〜{MAX_WAIT_SEC}（秒）"));
                }
                wait_sec = Some(w);
            }
            _ => return Err(format!("知らない引数 {k}")),
        }
    }
    let bind = bind.ok_or("--bind が要る")?;
    if let Some(text) = &code {
        if explicit_link {
            return Err("--code は --peer・--session・--secret と一緒に使わない".to_string());
        }
        let c = code::read_initiator(text, now_unix)
            .map_err(|e| format!("--code の接続コードが読めない: {e:?}"))?;
        session_id = c.session_id;
        secret = c.secret.ok_or("--code の接続コードに秘密が無い")?;
        peers = c.candidates.as_slice().iter().map(|x| x.addr).collect();
    }
    let (&peer, extra_peers) = peers.split_first().ok_or("--peer か --code が要る")?;
    let scope = scope.unwrap_or_default();
    guard::check_bind(bind, scope, "受信ソケット")?;
    for &p in &peers {
        guard::check_peer(p, scope)?;
    }
    if let Some(s) = stun {
        guard::check_peer(s, scope)?;
    }
    let spectate = match spectate_host {
        Some(sb) => {
            guard::check_bind(sb, scope, "観戦の受信ソケット")?;
            if sb.port() == 0 {
                return Err(
                    "--spectate-host のポートは 0 にできない（観戦用の接続コードに載せるアドレスになる）"
                        .to_string(),
                );
            }
            if sb == bind {
                return Err(
                    "--spectate-host が --bind と同じ（別の受信ソケットにする）".to_string()
                );
            }
            let sid = spectate_session.unwrap_or(DEFAULT_SPECTATE_SESSION);
            let ssec = spectate_secret.unwrap_or(DEFAULT_SPECTATE_SECRET);
            // 対戦と同じ session・秘密だと、観戦者が対戦の鍵を作れてしまう（`spectate::HostConfig`）。
            if sid == session_id || ssec == secret {
                return Err(
                    "観戦用の session・秘密が対戦のものと同じ（別の値にする。同じだと観戦者が対戦の鍵を作れる）"
                        .to_string(),
                );
            }
            Some(SpectateCli {
                bind: sb,
                session_id: sid,
                secret: ssec,
                // 既定は走るたびに新しい種（起こし直しで同じ鍵・番号 0 から数え直しにならない）。固定は明示したときだけ。
                key_seed: if spectate_fixed_key {
                    crate::keys::spectate_host_seed(&ssec, sid)
                } else {
                    crate::keys::fresh_seed()
                },
            })
        }
        None if spectate_session.is_some() || spectate_secret.is_some() || spectate_fixed_key => {
            return Err(
                "--spectate-session・--spectate-secret・--spectate-fixed-key は --spectate-host と一緒にだけ書ける"
                    .to_string(),
            );
        }
        None => None,
    };
    fault.validate()?;
    if follow && (segs.is_some() || frames.is_some()) {
        return Err("--follow は --segs・--frames と一緒に使わない".to_string());
    }
    let (segs, frames) = (segs.unwrap_or(2), frames.unwrap_or(600));
    if segs == 0 || frames == 0 {
        return Err("--segs と --frames は 1 以上".to_string());
    }
    // rollback の引数は、書いたつもりで効かない形を断る（`sim::run_opts` も同じ組を断る）。
    let mode_wish = match mode {
        Mode::Rollback => {
            if !lat_script.is_empty() {
                return Err(
                    "--mode rollback は --lat と一緒に使わない（latency の切替は delay 方式だけ）"
                        .to_string(),
                );
            }
            ModeWish::rollback(rb_delay.unwrap_or(rollback::DEFAULT_DELAY))
        }
        Mode::Delay => {
            if rb_delay.is_some() || rb_ahead.is_some() || rb_sync.is_some() {
                return Err(
                    "--rb-delay・--rb-ahead・--rb-sync は --mode rollback と一緒にだけ書ける"
                        .to_string(),
                );
            }
            ModeWish::DELAY
        }
    };
    // 既定の打ち切り: マッチのフレームの 3 倍の時間 ＋ 60 秒（接続・読み込み・終わりの待ちの分）。
    // 付いていく形はマッチの長さが分からないので `FOLLOW_MAX_MS`。マッチの時間はペース（`--fps`）で決まる。
    // `--wait-sec` を書いたら、既定にはその分を足す（繋がるのが待つ長さの終わり近くでも、マッチの分が残るように）。
    let play_ms = u64::from(segs) * u64::from(frames) * 1_000 / fps;
    let wait_ms = wait_sec.map(|w| u64::from(w) * 1_000);
    let max_ms = match max_sec {
        Some(s) => u64::from(s) * 1_000,
        None if follow => FOLLOW_MAX_MS + wait_ms.unwrap_or(0),
        None => play_ms * 3 + 60_000 + wait_ms.unwrap_or(0),
    };
    // 待つ長さが打ち切りより長ければ断る。黙って打ち切りで切ると、「長く待つ」つもりの回が 10 秒で手で合わせ損ねた
    // 回と同じ「打ち切り」の終わり方になり、どちらの引数が効いたのかが記録から分からないため。
    if let (Some(w), Some(m)) = (wait_sec, max_sec)
        && w > m
    {
        return Err(format!(
            "--wait-sec {w} が --max-sec {m} より長い（待つ間に打ち切りで切れる。--max-sec を延ばすか省く）"
        ));
    }
    Ok(Parsed::Run(Box::new(Cli {
        bind,
        sim: SimConfig {
            peer,
            extra_peers: extra_peers.to_vec(),
            secret,
            stun,
            name,
            session_id,
            role_nonce,
            latency,
            host_seat,
            seed,
            follow,
            word_mask,
            life,
            poll_ms: 1,
            segs,
            frames_per_seg: frames,
            load_ms,
            lat_script,
            fault,
            max_ms: u32::try_from(max_ms).unwrap_or(u32::MAX),
        },
        menu,
        log_dir,
        fps,
        wait_ms: wait_ms.map(|w| u32::try_from(w).unwrap_or(u32::MAX)),
        scope,
        spectate,
        mode: mode_wish,
        ahead: rb_ahead.unwrap_or(rollback::DEFAULT_AHEAD),
        time_sync: rb_sync.unwrap_or(true),
    })))
}

/// 観戦用の接続コードを書く（`--spectate-host` を開いたとき、起動時に 1 行出す）。候補は観戦のポートのアドレス 1 つ、有効期限は
/// 接続コードの既定（`code::DEFAULT_TTL_SECS`）。
pub fn spectate_code(s: &SpectateCli, name: &str, now_unix: u64) -> Result<String, String> {
    let candidates = Candidates::from_addrs(&[s.bind])
        .map_err(|e| format!("観戦の受信ソケット {} を候補にできない: {e:?}", s.bind))?;
    code::write_spectate(
        &code::Code {
            side: code::Side::Spectate,
            session_id: s.session_id,
            secret: Some(s.secret),
            name: Name::new(name),
            nat: higanr_core::nat::NatMark::UNKNOWN,
            candidates,
            expires: now_unix.saturating_add(code::DEFAULT_TTL_SECS),
        },
        // 観戦の配る側の公開鍵（`s.key_seed` の公開鍵 —— 配る側のセッションが使う秘密鍵と対）。
        &Ephemeral::from_seed(&s.key_seed).public(),
    )
    .map_err(|e| format!("観戦用の接続コードを書けない: {e:?}"))
}

/// `--watch`（観戦者の役）の形。書いたつもりの引数が黙って使われないよう、知らない引数は断る。
fn parse_watch(args: &[String], default_nonce: u64, now_unix: u64) -> Result<Parsed, String> {
    let mut bind = None;
    let mut peers: Vec<SocketAddr> = Vec::new();
    let mut name = "peer_sim".to_string();
    let mut session_id: Option<u32> = None;
    let mut secret: Option<Secret> = None;
    let mut code_text: Option<String> = None;
    let mut nonce = default_nonce;
    let mut scope: Option<Scope> = None;
    let mut start_after_ms: u32 = 0;
    let mut silent_ms: u32 = DEFAULT_SILENT_END_MS;
    let mut max_ms: u32 = DEFAULT_WATCH_MAX_MS;
    let mut log_dir = None;
    // --code と一緒に使えない引数を見たか。
    let mut explicit_link = false;
    let mut it = args.iter();
    while let Some(k) = it.next() {
        match k.as_str() {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--watch" => continue,
            _ => {}
        }
        let v = it
            .next()
            .ok_or_else(|| format!("{k} の後に値が要る"))?
            .as_str();
        match k.as_str() {
            "--bind" => bind = Some(addr(v, k)?),
            "--peer" => {
                peers.push(addr(v, k)?);
                explicit_link = true;
            }
            "--session" => {
                session_id = Some(num_in(v, k)?);
                explicit_link = true;
            }
            "--secret" => {
                secret = Some(Secret::from_hex(v)?);
                explicit_link = true;
            }
            "--code" => {
                if code_text.replace(v.to_string()).is_some() {
                    return Err("--code は 1 回だけ".to_string());
                }
            }
            "--name" => name = v.to_string(),
            "--nonce" => nonce = num(v, k)?,
            "--scope" => {
                if scope.is_some() {
                    return Err("--scope は 1 回だけ".to_string());
                }
                scope = Some(
                    Scope::parse(v)
                        .ok_or_else(|| format!("--scope の値 {v:?} は {}", Scope::NAMES))?,
                );
            }
            "--start-after-ms" => start_after_ms = num_in(v, k)?,
            "--silent-sec" => {
                let s: u32 = num_in(v, k)?;
                if !(1..=3_600).contains(&s) {
                    return Err("--silent-sec は 1〜3600（秒）".to_string());
                }
                silent_ms = s * 1_000;
            }
            "--max-sec" => {
                let s: u32 = num_in(v, k)?;
                if !(1..=86_400).contains(&s) {
                    return Err("--max-sec は 1〜86400（秒）".to_string());
                }
                max_ms = s * 1_000;
            }
            "--log-dir" => log_dir = Some(PathBuf::from(v)),
            other => {
                return Err(format!(
                    "--watch は --bind --peer --code --session --secret --name --nonce --scope --start-after-ms --silent-sec --max-sec --log-dir だけと一緒に書ける（{other} は使えない）"
                ));
            }
        }
    }
    let bind = bind.ok_or("--bind が要る")?;
    let (session_id, secret, host_public) = if let Some(text) = &code_text {
        if explicit_link {
            return Err("--code は --peer・--session・--secret と一緒に使わない".to_string());
        }
        // 観戦用のコードだけを読む（対戦用のコードは `WrongSide` で断る。取り違えを通さない）。
        let s = code::read_spectate(text, now_unix)
            .map_err(|e| format!("--code の観戦用の接続コードが読めない: {e:?}"))?;
        let c = s.code;
        peers = c.candidates.as_slice().iter().map(|x| x.addr).collect();
        (
            c.session_id,
            c.secret.ok_or("--code の接続コードに秘密が無い")?,
            s.host_public,
        )
    } else {
        // コードを使わない経路（配る側が `--spectate-fixed-key` の検査）: 配る側と同じ式で公開鍵を作る（`keys::spectate_host_public`）。
        // 配る側が既定（走るたびの種）だとこの公開鍵は合わず、流れを開けない。
        let session_id = session_id.unwrap_or(DEFAULT_SPECTATE_SESSION);
        let secret = secret.unwrap_or(DEFAULT_SPECTATE_SECRET);
        (
            session_id,
            secret,
            crate::keys::spectate_host_public(&secret, session_id),
        )
    };
    if peers.is_empty() {
        return Err("--peer か --code が要る（ホストの観戦の受信ソケット）".to_string());
    }
    let scope = scope.unwrap_or_default();
    guard::check_bind(bind, scope, "受信ソケット")?;
    for &p in &peers {
        guard::check_peer(p, scope)?;
    }
    Ok(Parsed::Watch(Box::new(WatchCli {
        bind,
        cfg: WatchConfig {
            host: peers,
            session_id,
            secret,
            host_public,
            name,
            nonce,
            fault: FaultConfig::default(),
            start_after_ms,
            silent_end_ms: silent_ms,
            max_ms,
        },
        scope,
        log_dir,
    })))
}

/// `--rewrite-spectate-code IN OUT --candidate ADDR [--scope S] [--overwrite]` の形。走らせる引数は受けない（書いたつもりの引数が
/// 黙って使われないのを避ける）。コードを読む・書く・ガードは `spectate_code::rewrite`、ファイルの読み書きは `main.rs`。
fn parse_rewrite(args: &[String]) -> Result<Parsed, String> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut candidate: Option<SocketAddr> = None;
    let mut scope: Option<Scope> = None;
    let mut overwrite = false;
    let mut it = args.iter().skip(1).peekable();
    while files.len() < 2 {
        match it.next_if(|a| !a.starts_with("--")) {
            Some(f) => files.push(PathBuf::from(f)),
            None => break,
        }
    }
    while let Some(k) = it.next() {
        match k.as_str() {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--overwrite" => overwrite = true,
            "--candidate" => {
                let v = it.next().ok_or("--candidate の後に値が要る")?;
                if candidate.replace(addr(v, k)?).is_some() {
                    return Err("--candidate は 1 回だけ".to_string());
                }
            }
            "--scope" => {
                let v = it.next().ok_or("--scope の後に値が要る")?;
                if scope.is_some() {
                    return Err("--scope は 1 回だけ".to_string());
                }
                scope = Some(
                    Scope::parse(v)
                        .ok_or_else(|| format!("--scope の値 {v:?} は {}", Scope::NAMES))?,
                );
            }
            other => {
                return Err(format!(
                    "--rewrite-spectate-code は IN OUT と --candidate --scope --overwrite だけと一緒に書ける（{other} は使えない）"
                ));
            }
        }
    }
    let [input, output] = <[PathBuf; 2]>::try_from(files).map_err(|_| {
        "--rewrite-spectate-code の後に、元のコードのファイルと出力先の 2 つが要る".to_string()
    })?;
    if input == output {
        return Err("入力と出力が同じファイル（元のコードを上書きしない）".to_string());
    }
    let candidate = candidate.ok_or("--candidate が要る（中継の観戦者向けの受信ソケット）")?;
    let scope = scope.unwrap_or_default();
    guard::check_peer(candidate, scope).map_err(|e| format!("--candidate: {e}"))?;
    Ok(Parsed::RewriteSpectateCode(Box::new(RewriteCli {
        input,
        output,
        candidate,
        scope,
        overwrite,
    })))
}

/// `--stun-probe A B` の形（`--bind` と `--scope internet` だけと一緒に書ける）。外の STUN サーバーへ問い合わせる道具なので、
/// 範囲を広げたことを引数に書かせ（既定の loopback のままでは動かさない）、走らせる引数（--peer など）は受けない
/// （書いたつもりの引数が黙って使われないのを避ける）。
fn parse_stun_probe(args: &[String]) -> Result<Parsed, String> {
    let mut bind = None;
    let mut scope: Option<Scope> = None;
    let mut targets: Option<(String, String)> = None;
    let mut it = args.iter();
    while let Some(k) = it.next() {
        match k.as_str() {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--bind" => {
                let v = it.next().ok_or("--bind の後に値が要る")?;
                if bind.replace(addr(v, k)?).is_some() {
                    return Err("--bind は 1 回だけ".to_string());
                }
            }
            "--scope" => {
                let v = it.next().ok_or("--scope の後に値が要る")?;
                if scope.is_some() {
                    return Err("--scope は 1 回だけ".to_string());
                }
                scope = Some(
                    Scope::parse(v)
                        .ok_or_else(|| format!("--scope の値 {v:?} は {}", Scope::NAMES))?,
                );
            }
            "--stun-probe" => {
                let (Some(a), Some(b)) = (it.next(), it.next()) else {
                    return Err(
                        "--stun-probe の後に宛先が 2 つ要る（--stun-probe A B）".to_string()
                    );
                };
                if targets.replace((a.clone(), b.clone())).is_some() {
                    return Err("--stun-probe は 1 回だけ".to_string());
                }
            }
            other => {
                return Err(format!(
                    "--stun-probe は --bind と --scope internet だけと一緒に書ける（{other} は使えない）"
                ));
            }
        }
    }
    if scope != Some(Scope::Internet) {
        return Err(format!(
            "--stun-probe は --scope internet と一緒にだけ書ける（外の STUN サーバーへ問い合わせる道具。範囲を広げたことを引数に書く。今の範囲: {}）",
            scope.map_or("書いていない（既定 loopback）", Scope::name)
        ));
    }
    let bind = bind.ok_or(
        "--bind が要る（自分の受信ソケット。--stun-probe の問い合わせをこの受信ソケットから出す）",
    )?;
    guard::check_bind(bind, Scope::Internet, "受信ソケット")?;
    let (a, b) = targets.ok_or("--stun-probe の宛先が要る")?;
    if a == b {
        return Err(format!(
            "--stun-probe の宛先 2 つが同じ {a:?}（別々の STUN サーバーを書く）"
        ));
    }
    Ok(Parsed::StunProbe(Box::new(stun_probe::Args {
        bind,
        scope: Scope::Internet,
        targets: [
            stun_probe::Target::parse(&a, Scope::Internet)?,
            stun_probe::Target::parse(&b, Scope::Internet)?,
        ],
    })))
}

/// `--stun-fake ADDR` の形（`--scope` と `--max-sec` だけと一緒に書ける）。範囲は `--scope lan` と書いたときだけ広がる
/// （既定の loopback は今までの偽の STUN と同じ）。受信ソケットは範囲のガードを通す（外のアドレス・未指定アドレスは断る）。走らせる引数
/// （--peer など）は受けない（書いたつもりの引数が黙って使われないのを避ける）。
fn parse_stun_fake(args: &[String]) -> Result<Parsed, String> {
    let mut bind = None;
    let mut scope: Option<Scope> = None;
    let mut max_sec: Option<u32> = None;
    let mut it = args.iter();
    while let Some(k) = it.next() {
        match k.as_str() {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--stun-fake" => {
                let v = it
                    .next()
                    .ok_or("--stun-fake の後に受信ソケットのアドレスが要る")?;
                if bind.replace(addr(v, k)?).is_some() {
                    return Err("--stun-fake は 1 回だけ".to_string());
                }
            }
            "--scope" => {
                let v = it.next().ok_or("--scope の後に値が要る")?;
                if scope.is_some() {
                    return Err("--scope は 1 回だけ".to_string());
                }
                scope = Some(
                    Scope::parse(v)
                        .ok_or_else(|| format!("--scope の値 {v:?} は {}", Scope::NAMES))?,
                );
            }
            "--max-sec" => {
                let v = it.next().ok_or("--max-sec の後に値が要る")?;
                if max_sec.is_some() {
                    return Err("--max-sec は 1 回だけ".to_string());
                }
                let n: u32 = num_in(v, k)?;
                if !(1..=stun_fake::MAX_MAX_SEC).contains(&n) {
                    return Err(format!("--max-sec は 1〜{}（秒）", stun_fake::MAX_MAX_SEC));
                }
                max_sec = Some(n);
            }
            other => {
                return Err(format!(
                    "--stun-fake は --scope と --max-sec だけと一緒に書ける（{other} は使えない）"
                ));
            }
        }
    }
    let scope = scope.unwrap_or_default();
    let bind = bind.ok_or("--stun-fake の受信ソケットのアドレスが要る")?;
    guard::check_bind(bind, scope, "受信ソケット")?;
    Ok(Parsed::StunFake(Box::new(stun_fake::Args {
        bind,
        scope,
        max_sec: max_sec.unwrap_or(stun_fake::DEFAULT_MAX_SEC),
    })))
}

const SHAPE_FAULT_KEYS: [&str; 11] = [
    "drop",
    "delay",
    "dup",
    "delay-max",
    "one-way-ms",
    "jitter-ms",
    "jitter-shape",
    "fault-seed",
    "drop-every",
    "drop-run",
    "drop-after",
];

/// B→A の乱数の seed を A→B と別の列にするための混ぜ値（`--b2a-fault-seed` を書かなかったとき）。値に意味は無い。
/// 両方向が同じ seed だと、同じ通番に同じ運命が出て「両方向が同時に落ちる」形に偏るため。
const B2A_SEED_MIX: u64 = 0x5DEE_CE66_D1CE_4E5B;

fn set_fault(cfg: &mut FaultConfig, key: &str, v: &str) -> Result<(), String> {
    let k = format!("--{key}");
    match key {
        "drop" => cfg.drop_pct = pct(v, &k)?,
        "delay" => cfg.delay_pct = pct(v, &k)?,
        "dup" => cfg.dup_pct = pct(v, &k)?,
        "delay-max" => cfg.delay_max_ms = num_in(v, &k)?,
        "one-way-ms" => cfg.one_way_ms = num_in(v, &k)?,
        "jitter-ms" => cfg.jitter_ms = num_in(v, &k)?,
        "jitter-shape" => {
            cfg.jitter_shape = fault::JitterShape::parse(v)
                .ok_or_else(|| format!("{k} の値 {v:?} は {}", fault::JitterShape::NAMES))?
        }
        "fault-seed" => cfg.seed = num(v, &k)?,
        "drop-every" => cfg.drop_every = num_in(v, &k)?,
        "drop-run" => cfg.drop_run = num_in(v, &k)?,
        "drop-after" => cfg.drop_after = num(v, &k)?,
        _ => return Err(format!("知らない乱れの項目 {key}")),
    }
    Ok(())
}

/// `--shape`（遅延とロスを揺らす中継。`shape`）の形。`--scope` は受けない（loopback だけ）。書いたつもりの引数が
/// 黙って使われないよう、知らない引数は断る。乱れは書き順に依らず「両方向の指定 → 向きごとの指定」の順に入れる。
fn parse_shape(args: &[String]) -> Result<Parsed, String> {
    let mut a = None;
    let mut b = None;
    let mut via_a = None;
    let mut via_b = None;
    let mut max_sec: Option<u32> = None;
    let mut faults: Vec<(Option<Dir>, String, String)> = Vec::new();
    let mut it = args.iter();
    while let Some(k) = it.next() {
        if k == "--help" || k == "-h" {
            return Ok(Parsed::Help);
        }
        if k == "--shape" {
            continue;
        }
        let v = it
            .next()
            .ok_or_else(|| format!("{k} の後に値が要る"))?
            .as_str();
        let once = |slot: &mut Option<SocketAddr>| -> Result<(), String> {
            if slot.replace(addr(v, k)?).is_some() {
                return Err(format!("{k} は 1 回だけ"));
            }
            Ok(())
        };
        match k.as_str() {
            "--a" => once(&mut a)?,
            "--b" => once(&mut b)?,
            "--via-a" => once(&mut via_a)?,
            "--via-b" => once(&mut via_b)?,
            "--max-sec" => {
                if max_sec.is_some() {
                    return Err("--max-sec は 1 回だけ".to_string());
                }
                max_sec = Some(num_in(v, k)?);
            }
            other => {
                let (dir, key) = if let Some(r) = other.strip_prefix("--a2b-") {
                    (Some(Dir::AtoB), r)
                } else if let Some(r) = other.strip_prefix("--b2a-") {
                    (Some(Dir::BtoA), r)
                } else {
                    (None, other.strip_prefix("--").unwrap_or(other))
                };
                if !SHAPE_FAULT_KEYS.contains(&key) {
                    return Err(format!(
                        "--shape は --a --b --via-a --via-b --max-sec と乱れ（--drop --delay --dup --delay-max --one-way-ms --jitter-ms --jitter-shape --fault-seed --drop-every --drop-run --drop-after。先頭に --a2b- / --b2a- を付けると向きごと）だけと一緒に書ける（{other} は使えない）"
                    ));
                }
                faults.push((dir, key.to_string(), v.to_string()));
            }
        }
    }
    let base = FaultConfig {
        seed: 1,
        ..FaultConfig::default()
    };
    let (mut a2b, mut b2a) = (base, base);
    for (_, key, v) in faults.iter().filter(|f| f.0.is_none()) {
        set_fault(&mut a2b, key, v)?;
        set_fault(&mut b2a, key, v)?;
    }
    let own_b2a_seed = faults
        .iter()
        .any(|(d, k, _)| *d == Some(Dir::BtoA) && k == "fault-seed");
    if !own_b2a_seed {
        b2a.seed = a2b.seed ^ B2A_SEED_MIX;
    }
    for (dir, key, v) in faults.iter().filter(|f| f.0.is_some()) {
        let cfg = match dir {
            Some(Dir::AtoB) => &mut a2b,
            _ => &mut b2a,
        };
        set_fault(cfg, key, v)?;
    }
    let args = shape::Args {
        a: a.ok_or("--a が要る（A の ini の bind）")?,
        b: b.ok_or("--b が要る（B の ini の bind）")?,
        via_a: via_a.ok_or("--via-a が要る（A の ini の peer に書く、A 向けの受信ソケット）")?,
        via_b: via_b.ok_or("--via-b が要る（B の ini の peer に書く、B 向けの受信ソケット）")?,
        a2b,
        b2a,
        max_sec: max_sec.unwrap_or(shape::DEFAULT_MAX_SEC),
    };
    // アドレスのガード（loopback だけ）・重なり・乱れの範囲・秒数は `Args::check` が見る（走らせるときにも見る）。
    args.check()?;
    Ok(Parsed::Shape(Box::new(args)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    fn run_of(s: &str) -> Cli {
        match parse(&args(s), 99) {
            Ok(Parsed::Run(c)) => *c,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn defaults_and_values() {
        let c = run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002");
        assert_eq!(c.bind, "127.0.0.1:7001".parse().unwrap());
        assert_eq!(c.sim.session_id, DEFAULT_SESSION);
        assert_eq!(c.sim.role_nonce, 99);
        assert_eq!(
            (c.sim.latency, c.sim.segs, c.sim.frames_per_seg),
            (2, 2, 600)
        );
        assert_eq!(c.sim.host_seat, Seat::P1);
        assert_eq!(c.sim.fault.drop_pct, 0);
        let c = run_of(
            "--bind [::1]:1 --peer [::1]:2 --session 0x10 --nonce 7 --latency 9 --host-seat 2 \
             --lat 1:30:4 --lat 0:5:0 --drop 20 --delay 10 --dup 5 --delay-max 40 --drop-every 7 --drop-run 3 --max-sec 5",
        );
        assert_eq!(c.sim.session_id, 16);
        assert_eq!(c.sim.role_nonce, 7);
        assert_eq!(c.sim.latency, 9);
        assert_eq!(c.sim.host_seat, Seat::P2);
        assert_eq!(
            c.sim.lat_script,
            vec![
                LatStep {
                    seg: 1,
                    frame: 30,
                    delay: 4
                },
                LatStep {
                    seg: 0,
                    frame: 5,
                    delay: 0
                }
            ]
        );
        assert_eq!(
            (
                c.sim.fault.drop_pct,
                c.sim.fault.delay_pct,
                c.sim.fault.dup_pct
            ),
            (20, 10, 5)
        );
        assert_eq!((c.sim.fault.drop_every, c.sim.fault.drop_run), (7, 3));
        assert_eq!(c.sim.max_ms, 5_000);
        assert!(!c.sim.follow);
        assert_eq!(
            (c.sim.word_mask, c.sim.life, c.sim.poll_ms),
            (0xFFFF, 10, 1)
        );
        let c = run_of(
            "--bind 127.0.0.1:7001 --follow --peer 127.0.0.1:7002 --load-ms 5 --mask 0xC4 --life 1",
        );
        assert!(c.sim.follow);
        assert_eq!(c.sim.load_ms, 5);
        assert_eq!(c.sim.max_ms, 1_800_000);
        assert_eq!((c.sim.word_mask, c.sim.life), (0xC4, 1));
        assert_eq!(c.menu, None);
        let c = run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --follow --lat 1:300:4");
        assert!(c.sim.follow);
        assert_eq!(
            c.sim.lat_script,
            vec![LatStep {
                seg: 1,
                frame: 300,
                delay: 4
            }]
        );
        let c = run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --menu m.txt");
        assert_eq!(c.menu, Some(PathBuf::from("m.txt")));
    }

    #[test]
    fn mode_and_rollback_values() {
        let base = "--bind 127.0.0.1:7001 --peer 127.0.0.1:7002";
        let c = run_of(base);
        assert_eq!(
            (c.mode, c.ahead),
            (ModeWish::DELAY, rollback::DEFAULT_AHEAD)
        );
        let c = run_of(&format!("{base} --mode rollback --latency 5"));
        assert_eq!(
            c.mode,
            ModeWish::rollback(1),
            "D の既定は 1（--latency と別）"
        );
        assert_eq!(c.sim.latency, 5);
        let c = run_of(&format!(
            "{base} --mode rollback --rb-delay 3 --rb-ahead 12"
        ));
        assert_eq!((c.mode, c.ahead), (ModeWish::rollback(3), 12));
        assert!(c.time_sync, "時刻合わせの既定は使う");
        assert!(run_of(&format!("{base} --mode rollback --rb-sync on")).time_sync);
        assert!(!run_of(&format!("{base} --mode rollback --rb-sync off")).time_sync);
        let c = run_of(&format!("{base} --mode delay"));
        assert_eq!(c.mode, ModeWish::DELAY);
        let c = run_of(&format!("{base} --mode rollback --follow"));
        assert!(c.sim.follow && c.mode == ModeWish::rollback(1));
        for bad in [
            "--mode rollback --rb-sync yes",
            "--mode delay --rb-sync off",
            "--rb-sync off",
            "--mode Rollback",
            "--mode 1",
            "--mode rollback --rb-delay 10",
            "--mode rollback --rb-ahead 0",
            "--mode rollback --rb-ahead 17",
            "--mode rollback --lat 0:5:3",
            "--rb-delay 2",
            "--mode delay --rb-ahead 4",
        ] {
            assert!(
                parse(&args(&format!("{base} {bad}")), 99).is_err(),
                "{bad} を通した"
            );
        }
    }

    #[test]
    fn fps_and_one_way_delay() {
        let c = run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002");
        assert_eq!((c.fps, c.sim.fault.one_way_ms), (60, 0));
        assert_eq!(c.sim.max_ms, 120_000);
        let c = run_of(
            "--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --fps 240 --one-way-ms 80 --latency 4",
        );
        assert_eq!((c.fps, c.sim.fault.one_way_ms, c.sim.latency), (240, 80, 4));
        assert_eq!(c.sim.max_ms, 75_000);
        let c = run_of(
            "--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --one-way-ms 1000 --delay 10 --delay-max 5",
        );
        assert_eq!(
            (
                c.sim.fault.one_way_ms,
                c.sim.fault.delay_pct,
                c.sim.fault.delay_max_ms
            ),
            (1_000, 10, 5)
        );
        for s in [
            "--fps 0",
            "--fps 1001",
            "--fps x",
            "--one-way-ms 1001",
            "--one-way-ms -1",
        ] {
            let a = format!("--bind 127.0.0.1:1 --peer 127.0.0.1:2 {s}");
            assert!(parse(&args(&a), 1).is_err(), "{s} を通した");
        }
        let c = run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002");
        assert_eq!(c.sim.fault.claim_ver, None);
        let c = run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --proto-ver 0");
        assert_eq!(c.sim.fault.claim_ver, Some(0));
        for s in [
            format!("--proto-ver {}", higanr_core::wire::PROTO_VER),
            "--proto-ver 256".to_string(),
            "--proto-ver x".to_string(),
        ] {
            let a = format!("--bind 127.0.0.1:1 --peer 127.0.0.1:2 {s}");
            assert!(parse(&args(&a), 1).is_err(), "{s} を通した");
        }
        let c = run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002");
        assert_eq!(
            (c.sim.fault.jitter_ms, c.sim.fault.jitter_shape),
            (0, fault::JitterShape::Uniform)
        );
        let c = run_of(
            "--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --jitter-ms 1000 --jitter-shape spiky",
        );
        assert_eq!(
            (c.sim.fault.jitter_ms, c.sim.fault.jitter_shape),
            (1_000, fault::JitterShape::Spiky)
        );
        for s in [
            "--jitter-ms 1001",
            "--jitter-ms -1",
            "--jitter-ms x",
            "--jitter-shape gauss",
        ] {
            let a = format!("--bind 127.0.0.1:1 --peer 127.0.0.1:2 {s}");
            assert!(parse(&args(&a), 1).is_err(), "{s} を通した");
        }
        assert!(
            parse(&args("--bind 127.0.0.1:1 --peer 127.0.0.1:2 --fps 1000"), 1).is_ok()
                && parse(&args("--bind 127.0.0.1:1 --peer 127.0.0.1:2 --fps 1"), 1).is_ok()
        );
    }

    #[test]
    fn wait_sec_sets_the_retry_window_only_when_written() {
        let c = run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002");
        assert_eq!(c.wait_ms, None);
        assert_eq!(c.sim.max_ms, 120_000);
        let c = run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --wait-sec 600");
        assert_eq!(c.wait_ms, Some(600_000));
        assert_eq!(c.sim.max_ms, 720_000);
        let c = run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --follow --wait-sec 3600");
        assert_eq!(c.wait_ms, Some(3_600_000));
        assert_eq!(c.sim.max_ms, 1_800_000 + 3_600_000);
        let c = run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --wait-sec 30 --max-sec 30");
        assert_eq!((c.wait_ms, c.sim.max_ms), (Some(30_000), 30_000));
        let c = run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --wait-sec 1");
        assert_eq!(c.wait_ms, Some(1_000));
        for s in [
            "--wait-sec 0",
            "--wait-sec 3601",
            "--wait-sec x",
            "--wait-sec -1",
            "--wait-sec 1.5",
            "--wait-sec",
            "--wait-sec 31 --max-sec 30",
            "--max-sec 30 --wait-sec 31",
        ] {
            let a = format!("--bind 127.0.0.1:1 --peer 127.0.0.1:2 {s}");
            assert!(parse(&args(&a), 1).is_err(), "{s} を通した");
        }
        let e = parse(
            &args("--bind 127.0.0.1:1 --peer 127.0.0.1:2 --wait-sec 31 --max-sec 30"),
            1,
        )
        .unwrap_err();
        assert!(e.contains("--max-sec"), "{e}");
    }

    #[test]
    fn secret_candidates_stun_and_code() {
        let c = run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002");
        assert_eq!(c.sim.secret, DEFAULT_SECRET);
        assert_eq!(DEFAULT_SECRET.to_hex(), "706565725f73696d2064656661756c74");
        assert!(c.sim.extra_peers.is_empty() && c.sim.stun.is_none());
        let c = run_of(
            "--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --peer [::1]:7003 --peer 127.0.0.9:7004 \
             --secret 000102030405060708090a0b0c0d0e0f --stun 127.0.0.1:3478",
        );
        assert_eq!(c.sim.peer, "127.0.0.1:7002".parse().unwrap());
        assert_eq!(
            c.sim.extra_peers,
            vec![
                "[::1]:7003".parse().unwrap(),
                "127.0.0.9:7004".parse().unwrap()
            ]
        );
        assert_eq!(c.sim.secret.to_hex(), "000102030405060708090a0b0c0d0e0f");
        assert_eq!(c.sim.stun, Some("127.0.0.1:3478".parse().unwrap()));

        let mut cands = higanr_core::candidate::Candidates::EMPTY;
        for a in ["127.0.0.1:47001", "127.0.0.2:47002"] {
            cands
                .push(higanr_core::candidate::Candidate::guess(a.parse().unwrap()).unwrap())
                .unwrap();
        }
        let secret = Secret([0x42; 16]);
        let code = code::Code {
            side: code::Side::Initiator,
            session_id: 0xCAFE_0001,
            secret: Some(secret),
            name: higanr_core::handshake::Name::new("dll"),
            nat: higanr_core::nat::NatMark::UNKNOWN,
            candidates: cands,
            expires: 1_000,
        };
        let text = code::write_initiator(&code).unwrap();
        let run = |s: String, now: u64| match parse_at(&args(&s), 1, now) {
            Ok(Parsed::Run(c)) => Ok(*c),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e),
        };
        let c = run(format!("--bind 127.0.0.1:7001 --code {text}"), 1_000).unwrap();
        assert_eq!((c.sim.session_id, c.sim.secret), (0xCAFE_0001, secret));
        assert_eq!(c.sim.peer, "127.0.0.1:47001".parse().unwrap());
        assert_eq!(c.sim.extra_peers, vec!["127.0.0.2:47002".parse().unwrap()]);
        let e = run(format!("--bind 127.0.0.1:7001 --code {text}"), 1_001).unwrap_err();
        assert!(e.contains("Expired"), "{e}");
        for extra in [
            "--peer 127.0.0.1:1",
            "--session 1",
            "--secret 00000000000000000000000000000000",
        ] {
            let e = run(format!("--bind 127.0.0.1:7001 --code {text} {extra}"), 0).unwrap_err();
            assert!(e.contains("一緒に使わない"), "{extra}: {e}");
        }
        let mut broken = text.clone();
        broken.replace_range(3..4, if &text[3..4] == "A" { "B" } else { "A" });
        assert!(run(format!("--bind 127.0.0.1:7001 --code {broken}"), 0).is_err());
        let mut lan = code;
        lan.candidates =
            higanr_core::candidate::Candidates::from_addrs(&["192.168.1.9:7000".parse().unwrap()])
                .unwrap();
        let t = code::write_initiator(&lan).unwrap();
        let e = run(format!("--bind 127.0.0.1:7001 --code {t}"), 0).unwrap_err();
        assert!(e.contains("scope = loopback"), "{e}");
        let c = run(format!("--bind 127.0.0.1:7001 --scope lan --code {t}"), 0).unwrap();
        assert_eq!(c.sim.peer, "192.168.1.9:7000".parse().unwrap());
        assert_eq!(c.scope, Scope::Lan);
        for s in [
            "--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --secret 00",
            "--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --stun 10.0.0.1:3478",
            "--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --peer 10.0.0.1:7003",
            "--bind 127.0.0.1:7001",
        ] {
            assert!(parse(&args(s), 1).is_err(), "{s} を通した");
        }
    }

    #[test]
    fn non_loopback_bind_and_peer_are_refused() {
        // (引数, 理由の文に出るはずの語)。範囲の外は範囲の名前を、意味の無いアドレスは「どの範囲でも」を言う。
        const OUT: &str = "scope = loopback";
        const NEVER: &str = "どのネットワークの範囲でも";
        for (s, why) in [
            ("--bind 0.0.0.0:7001 --peer 127.0.0.1:7002", NEVER),
            ("--bind 192.168.0.5:7001 --peer 127.0.0.1:7002", OUT),
            ("--bind 127.0.0.1:7001 --peer 192.168.0.6:7002", OUT),
            ("--bind 127.0.0.1:7001 --peer 203.0.113.9:7002", OUT),
            (
                "--bind 127.0.0.1:7001 --peer [::ffff:127.0.0.1]:7002",
                NEVER,
            ),
            ("--bind [::]:7001 --peer [::1]:7002", NEVER),
        ] {
            let e = parse(&args(s), 1).unwrap_err();
            assert!(e.contains(why), "{s}: {e}");
        }
        let e = parse(&args("--bind 127.0.0.1:1 --peer localhost:7002"), 1).unwrap_err();
        assert!(e.contains("名前は受けない"), "{e}");
    }

    #[test]
    fn scope_widens_the_gate_only_as_written() {
        assert_eq!(
            run_of("--bind 127.0.0.1:7001 --peer 127.0.0.1:7002").scope,
            Scope::Loopback
        );
        let c = run_of(
            "--scope lan --bind 192.168.1.5:7001 --peer 192.168.1.6:7002 --peer 10.1.2.3:7002 \
             --peer 172.31.0.1:7002 --peer [fe80::6]:7002 --stun 169.254.0.1:3478",
        );
        assert_eq!(c.scope, Scope::Lan);
        assert_eq!(c.sim.extra_peers.len(), 3);
        assert_eq!(c.sim.stun, Some("169.254.0.1:3478".parse().unwrap()));
        let c = run_of(
            "--bind 192.168.1.5:7001 --peer 203.0.113.9:7002 --peer [2001:db8::9]:7002 \
             --stun 198.51.100.1:3478 --scope internet",
        );
        assert_eq!(c.scope, Scope::Internet);
        assert_eq!(c.sim.peer, "203.0.113.9:7002".parse().unwrap());
        for s in [
            "--bind 192.168.1.5:7001 --peer 192.168.1.6:7002",
            "--scope loopback --bind 127.0.0.1:7001 --peer 192.168.1.6:7002",
            "--scope lan --bind 127.0.0.1:7001 --peer 203.0.113.9:7002",
            "--scope lan --bind 127.0.0.1:7001 --peer 172.32.0.1:7002",
            "--scope lan --bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --stun 198.51.100.1:3478",
            "--scope lan --bind 203.0.113.5:7001 --peer 127.0.0.1:7002",
            "--scope internet --bind 0.0.0.0:7001 --peer 203.0.113.9:7002",
            "--scope internet --bind 127.0.0.1:7001 --peer 224.0.0.251:5353",
            "--scope internet --bind 127.0.0.1:7001 --peer 255.255.255.255:7002",
            "--scope internet --bind 127.0.0.1:7001 --peer 203.0.113.9:0",
            "--scope internet --bind 127.0.0.1:7001 --peer [::ffff:203.0.113.9]:7002",
            "--scope LAN --bind 127.0.0.1:7001 --peer 127.0.0.1:7002",
            "--scope wan --bind 127.0.0.1:7001 --peer 127.0.0.1:7002",
            "--scope lan --scope lan --bind 127.0.0.1:7001 --peer 127.0.0.1:7002",
            "--bind 127.0.0.1:7001 --peer 127.0.0.1:7002 --scope",
        ] {
            assert!(parse(&args(s), 1).is_err(), "{s} を通した");
        }
        let e = parse(
            &args("--scope Lan --bind 127.0.0.1:1 --peer 127.0.0.1:2"),
            1,
        )
        .unwrap_err();
        assert!(
            e.contains("--scope") && e.contains("loopback / lan / internet"),
            "{e}"
        );
    }

    #[test]
    fn stun_fake_needs_scope_lan_to_widen() {
        let fake = |s: &str| match parse(&args(s), 1) {
            Ok(Parsed::StunFake(a)) => Ok(*a),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e),
        };
        let a = fake("--stun-fake 127.0.0.1:47900").unwrap();
        assert_eq!(
            (a.bind, a.scope, a.max_sec),
            (
                "127.0.0.1:47900".parse().unwrap(),
                Scope::Loopback,
                stun_fake::DEFAULT_MAX_SEC
            )
        );
        let a = fake("--scope lan --stun-fake 192.168.11.6:47900 --max-sec 60").unwrap();
        assert_eq!(
            (a.bind, a.scope, a.max_sec),
            ("192.168.11.6:47900".parse().unwrap(), Scope::Lan, 60)
        );
        assert!(fake("--scope lan --stun-fake 127.0.0.1:47900").is_ok());
        for s in [
            "--stun-fake 192.168.11.6:47900",
            "--scope loopback --stun-fake 10.0.0.5:47900",
        ] {
            let e = fake(s).unwrap_err();
            assert!(e.contains("scope = loopback"), "{s}: {e}");
        }
        for s in [
            "--scope lan --stun-fake 203.0.113.9:47900",
            "--scope lan --stun-fake 0.0.0.0:47900",
            "--scope lan --stun-fake [::ffff:192.168.11.6]:47900",
            "--scope lan",
            "--stun-fake 127.0.0.1:1 --stun-fake 127.0.0.1:2",
            "--stun-fake localhost:47900",
            "--stun-fake 127.0.0.1:47900 --peer 127.0.0.1:7002",
            "--stun-fake 127.0.0.1:47900 --max-sec 0",
            "--stun-fake 127.0.0.1:47900 --max-sec 7201",
            "--stun-fake 127.0.0.1:47900 --scope lan --scope lan",
            "--stun-fake 127.0.0.1:47900 --scope LAN",
        ] {
            assert!(fake(s).is_err(), "{s} を通した");
        }
    }

    #[test]
    fn stun_probe_needs_internet_scope_and_two_destinations() {
        let probe = |s: &str| match parse(&args(s), 1) {
            Ok(Parsed::StunProbe(a)) => Ok(*a),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e),
        };
        let a = probe(
            "--scope internet --bind 192.168.0.5:0 --stun-probe stun.cloudflare.com:3478 203.0.113.9:19302",
        )
        .unwrap();
        assert_eq!(a.bind, "192.168.0.5:0".parse().unwrap());
        assert_eq!(a.scope, Scope::Internet);
        assert_eq!(
            a.targets,
            [
                stun_probe::Target::Name {
                    host: "stun.cloudflare.com".to_string(),
                    port: 3478
                },
                stun_probe::Target::Addr("203.0.113.9:19302".parse().unwrap())
            ]
        );
        assert!(
            probe("--stun-probe a.example:1 b.example:2 --bind 127.0.0.1:0 --scope internet")
                .is_ok()
        );
        for s in [
            "--bind 127.0.0.1:0 --stun-probe a.example:1 b.example:2",
            "--scope lan --bind 127.0.0.1:0 --stun-probe a.example:1 b.example:2",
            "--scope loopback --bind 127.0.0.1:0 --stun-probe 127.0.0.1:1 127.0.0.1:2",
        ] {
            let e = probe(s).unwrap_err();
            assert!(e.contains("--scope internet"), "{s}: {e}");
        }
        for s in [
            "--scope internet --bind 127.0.0.1:0 --stun-probe a.example:1",
            "--scope internet --bind 127.0.0.1:0 --stun-probe",
            "--scope internet --bind 127.0.0.1:0 --stun-probe a.example:1 a.example:1",
            "--scope internet --bind 127.0.0.1:0 --stun-probe a.example:1 b.example:2 c.example:3",
            "--scope internet --stun-probe a.example:1 b.example:2",
            "--scope internet --bind 0.0.0.0:0 --stun-probe a.example:1 b.example:2",
            "--scope internet --bind localhost:0 --stun-probe a.example:1 b.example:2",
            "--scope internet --bind 127.0.0.1:0 --stun-probe a.example:0 b.example:2",
            "--scope internet --bind 127.0.0.1:0 --stun-probe 224.0.0.1:3478 b.example:2",
            "--scope internet --bind 127.0.0.1:0 --stun-probe a_b.example:1 b.example:2",
            "--scope internet --bind 127.0.0.1:0 --peer 127.0.0.1:2 --stun-probe a.example:1 b.example:2",
            "--scope internet --bind 127.0.0.1:0 --stun 127.0.0.1:3 --stun-probe a.example:1 b.example:2",
            "--scope internet --scope internet --bind 127.0.0.1:0 --stun-probe a.example:1 b.example:2",
        ] {
            assert!(probe(s).is_err(), "{s} を通した");
        }
        assert!(matches!(
            parse(
                &args("--bind 127.0.0.1:1 --peer 127.0.0.1:2 --scope internet"),
                1
            ),
            Ok(Parsed::Run(_))
        ));
        assert_eq!(
            parse(&args("--help --stun-probe a.example:1 b.example:2"), 1),
            Ok(Parsed::Help),
            "--help"
        );
    }

    #[test]
    fn rewrite_takes_two_files_a_candidate_and_stays_in_scope() {
        let ok = match parse(
            &args("--rewrite-spectate-code in.txt out.txt --candidate 127.0.0.1:47022"),
            1,
        ) {
            Ok(Parsed::RewriteSpectateCode(r)) => *r,
            other => panic!("{other:?}"),
        };
        assert_eq!(ok.input, PathBuf::from("in.txt"));
        assert_eq!(ok.output, PathBuf::from("out.txt"));
        assert_eq!(ok.candidate, "127.0.0.1:47022".parse().unwrap());
        assert_eq!((ok.scope, ok.overwrite), (Scope::Loopback, false));
        match parse(
            &args("--rewrite-spectate-code a b --overwrite --candidate 192.168.1.9:1 --scope lan"),
            1,
        ) {
            Ok(Parsed::RewriteSpectateCode(r)) => {
                assert_eq!((r.scope, r.overwrite), (Scope::Lan, true));
            }
            other => panic!("{other:?}"),
        }
        for s in [
            "--rewrite-spectate-code --candidate 127.0.0.1:1 a b",
            "--rewrite-spectate-code a b",
            "--rewrite-spectate-code a --candidate 127.0.0.1:1",
            "--rewrite-spectate-code a b c --candidate 127.0.0.1:1",
            "--rewrite-spectate-code a a --candidate 127.0.0.1:1",
            "--rewrite-spectate-code a b --candidate 192.168.1.9:1",
            "--rewrite-spectate-code a b --candidate 127.0.0.1:0",
            "--rewrite-spectate-code a b --candidate 127.0.0.1:1 --candidate 127.0.0.1:2",
            "--rewrite-spectate-code a b --candidate localhost:1",
            "--rewrite-spectate-code a b --candidate 127.0.0.1:1 --bind 127.0.0.1:2",
            "--rewrite-spectate-code a b --candidate 127.0.0.1:1 --scope moon",
        ] {
            assert!(parse(&args(s), 1).is_err(), "{s} を通した");
        }
        assert_eq!(
            parse(&args("--rewrite-spectate-code --help"), 1),
            Ok(Parsed::Help)
        );
    }

    #[test]
    fn bad_values_are_refused() {
        for s in [
            "--bind 127.0.0.1:1",
            "--peer 127.0.0.1:1",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --latency 10",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --lat 1:2",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --lat 1:2:10",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --drop 101",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --drop 60 --delay 50 --delay-max 5",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --host-seat 3",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --segs 0",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --what 1",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --seed",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --follow --segs 3",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --follow --frames 10",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --life 0",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --life 11",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --mask 0x10000",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --menu",
            "--bind 127.0.0.1:1 --peer 127.0.0.1:2 --menu a.txt --menu b.txt",
        ] {
            assert!(parse(&args(s), 1).is_err(), "{s} を通した");
        }
        assert_eq!(parse(&args("--help"), 1), Ok(Parsed::Help));
        assert_eq!(
            parse(&args("--compare a.csv b.txt"), 1),
            Ok(Parsed::Compare(
                PathBuf::from("a.csv"),
                PathBuf::from("b.txt")
            ))
        );
        assert!(parse(&args("--compare a.csv"), 1).is_err());
        assert!(parse(&args("--compare a.csv b.txt c"), 1).is_err());
        assert_eq!(
            parse(&args("--compare2 a.csv b.csv"), 1),
            Ok(Parsed::Compare2(
                PathBuf::from("a.csv"),
                PathBuf::from("b.csv")
            ))
        );
        assert!(parse(&args("--compare2 a.csv"), 1).is_err());
        assert!(parse(&args("--compare2 a.csv b.csv c"), 1).is_err());
    }

    fn shape_of(s: &str) -> Result<shape::Args, String> {
        match parse(&args(s), 1) {
            Ok(Parsed::Shape(a)) => Ok(*a),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e),
        }
    }

    const SHAPE_ADDRS: &str = "--shape --a 127.0.0.1:47001 --b 127.0.0.1:47002 --via-a 127.0.0.1:47011 --via-b 127.0.0.1:47012";

    #[test]
    fn shape_args_split_faults_per_direction() {
        let s = shape_of(SHAPE_ADDRS).unwrap();
        assert_eq!(
            (s.a, s.b, s.via_a, s.via_b),
            (
                "127.0.0.1:47001".parse().unwrap(),
                "127.0.0.1:47002".parse().unwrap(),
                "127.0.0.1:47011".parse().unwrap(),
                "127.0.0.1:47012".parse().unwrap()
            )
        );
        assert_eq!(s.max_sec, shape::DEFAULT_MAX_SEC);
        assert_eq!((s.a2b.drop_pct, s.b2a.drop_pct), (0, 0));
        assert_eq!(s.a2b.seed, 1);
        assert_ne!(s.a2b.seed, s.b2a.seed);

        for order in [
            "--one-way-ms 30 --drop 2 --a2b-drop 9 --b2a-one-way-ms 0",
            "--b2a-one-way-ms 0 --a2b-drop 9 --drop 2 --one-way-ms 30",
        ] {
            let s = shape_of(&format!("{SHAPE_ADDRS} {order}")).unwrap();
            assert_eq!((s.a2b.drop_pct, s.a2b.one_way_ms), (9, 30), "{order}: A→B");
            assert_eq!((s.b2a.drop_pct, s.b2a.one_way_ms), (2, 0), "{order}: B→A");
        }
        let s = shape_of(&format!(
            "{SHAPE_ADDRS} --fault-seed 5 --delay 10 --delay-max 40 --dup 5 --drop-every 7 --drop-run 3 --drop-after 900 --max-sec 60"
        ))
        .unwrap();
        for c in [s.a2b, s.b2a] {
            assert_eq!((c.delay_pct, c.delay_max_ms, c.dup_pct), (10, 40, 5));
            assert_eq!((c.drop_every, c.drop_run, c.drop_after), (7, 3, 900));
        }
        assert_eq!((s.a2b.seed, s.max_sec), (5, 60));
        assert_ne!(s.b2a.seed, 5);
        let s = shape_of(&format!("{SHAPE_ADDRS} --b2a-fault-seed 77")).unwrap();
        assert_eq!((s.a2b.seed, s.b2a.seed), (1, 77));

        for bad in [
            "--shape --a 127.0.0.1:1 --b 127.0.0.1:2 --via-a 127.0.0.1:3",
            "--shape --b 127.0.0.1:2 --via-a 127.0.0.1:3 --via-b 127.0.0.1:4",
            "--shape --a 127.0.0.1:1 --a 127.0.0.1:9 --b 127.0.0.1:2 --via-a 127.0.0.1:3 --via-b 127.0.0.1:4",
            "--shape --a 127.0.0.1:1 --b 127.0.0.1:2 --via-a 127.0.0.1:3 --via-b 127.0.0.1:3",
            "--shape --a 127.0.0.1:1 --b 127.0.0.1:1 --via-a 127.0.0.1:3 --via-b 127.0.0.1:4",
            "--shape --a 192.168.1.5:1 --b 127.0.0.1:2 --via-a 127.0.0.1:3 --via-b 127.0.0.1:4",
            "--shape --a 127.0.0.1:1 --b 127.0.0.1:2 --via-a 127.0.0.1:3 --via-b 8.8.8.8:4",
            "--shape --a 127.0.0.1:1 --b 127.0.0.1:2 --via-a 0.0.0.0:3 --via-b 127.0.0.1:4",
            "--shape --a localhost:1 --b 127.0.0.1:2 --via-a 127.0.0.1:3 --via-b 127.0.0.1:4",
        ] {
            assert!(shape_of(bad).is_err(), "{bad} を通した");
        }
        for extra in [
            "--scope lan",
            "--bind 127.0.0.1:9",
            "--peer 127.0.0.1:9",
            "--what 1",
            "--a2c-drop 5",
            "--a2b-what 5",
            "--drop 101",
            "--drop 60 --delay 50 --delay-max 5",
            "--one-way-ms 1001",
            "--max-sec 0",
            "--max-sec 7201",
            "--max-sec 5 --max-sec 6",
            "--drop",
        ] {
            assert!(
                shape_of(&format!("{SHAPE_ADDRS} {extra}")).is_err(),
                "{extra} を通した"
            );
        }
        assert!(
            shape_of(&format!(
                "{SHAPE_ADDRS} --b2a-drop 60 --b2a-delay 50 --b2a-delay-max 5"
            ))
            .is_err()
        );
        assert_eq!(parse(&args("--shape --help"), 1), Ok(Parsed::Help));
    }

    #[test]
    fn spectate_host_flags_are_separate_from_the_play_link_and_in_scope() {
        let base = "--bind 127.0.0.1:7001 --peer 127.0.0.1:7002";
        assert_eq!(run_of(base).spectate, None);
        let c = run_of(&format!("{base} --spectate-host 127.0.0.1:7100"));
        let s = c.spectate.clone().unwrap();
        assert_eq!(s.bind, "127.0.0.1:7100".parse().unwrap());
        assert_eq!(
            (s.session_id, s.secret),
            (DEFAULT_SPECTATE_SESSION, DEFAULT_SPECTATE_SECRET)
        );
        assert_ne!(s.session_id, c.sim.session_id, "既定が対戦と同じ session");
        assert_ne!(s.secret, c.sim.secret, "既定が対戦と同じ秘密");
        // 配る側の鍵の種: 既定は起こすたびに違う（同じ秘密・session で起こし直しても鍵が変わる）。固定は明示したときだけで、
        // そのときは式の種（観戦者が式で公開鍵を作る経路）。
        let again = run_of(&format!("{base} --spectate-host 127.0.0.1:7100"))
            .spectate
            .unwrap();
        assert_eq!((again.session_id, again.secret), (s.session_id, s.secret));
        assert_ne!(again.key_seed, s.key_seed, "起こし直しても同じ鍵の種");
        let fixed = run_of(&format!(
            "{base} --spectate-host 127.0.0.1:7100 --spectate-fixed-key"
        ))
        .spectate
        .unwrap();
        assert_eq!(
            fixed.key_seed,
            crate::keys::spectate_host_seed(&s.secret, s.session_id)
        );
        let c = run_of(&format!(
            "{base} --spectate-host 127.0.0.1:7100 --spectate-session 9 \
             --spectate-secret 000102030405060708090a0b0c0d0e0f"
        ));
        let s = c.spectate.unwrap();
        assert_eq!(s.session_id, 9);
        assert_eq!(s.secret.to_hex(), "000102030405060708090a0b0c0d0e0f");
        for (extra, why) in [
            (
                "--spectate-host 127.0.0.1:7100 --spectate-session 0x48475254",
                "同じ",
            ),
            (
                "--spectate-host 127.0.0.1:7100 --spectate-secret 706565725f73696d2064656661756c74",
                "同じ",
            ),
            ("--spectate-host 192.168.1.9:7100", "scope = loopback"),
            ("--spectate-host 0.0.0.0:7100", "どのネットワークの範囲でも"),
            ("--spectate-host 127.0.0.1:0", "ポートは 0"),
            ("--spectate-host 127.0.0.1:7001", "--bind と同じ"),
            ("--spectate-session 9", "--spectate-host と一緒に"),
            ("--spectate-fixed-key", "--spectate-host と一緒に"),
            (
                "--spectate-secret 000102030405060708090a0b0c0d0e0f",
                "--spectate-host と一緒に",
            ),
            (
                "--spectate-host 127.0.0.1:7100 --spectate-host 127.0.0.1:7101",
                "1 回だけ",
            ),
        ] {
            let e = parse(&args(&format!("{base} {extra}")), 1).unwrap_err();
            assert!(e.contains(why), "{extra}: {e}");
        }
        let s = SpectateCli {
            bind: "127.0.0.1:7100".parse().unwrap(),
            session_id: 0xABCD_0001,
            secret: Secret([0x33; 16]),
            key_seed: crate::keys::fresh_seed(),
        };
        let text = spectate_code(&s, "host", 1_000).unwrap();
        let sc = code::read_spectate(&text, 1_000).unwrap();
        // コードの公開鍵は配る側のセッションが使う種（`SpectateCli::key_seed`）の公開鍵と同じ。式で決めた固定の鍵（観戦用の秘密と session から）
        // とは別 —— 既定は走るたびの種なので、同じ引数で 2 回起こしても公開鍵が違う。
        assert_eq!(sc.host_public, Ephemeral::from_seed(&s.key_seed).public());
        assert_ne!(
            sc.host_public,
            crate::keys::spectate_host_public(&s.secret, s.session_id)
        );
        let c = sc.code;
        assert_eq!(
            (c.session_id, c.secret),
            (0xABCD_0001, Some(Secret([0x33; 16])))
        );
        assert_eq!(c.candidates.as_slice()[0].addr, s.bind);
        assert_eq!(c.expires, 1_000 + code::DEFAULT_TTL_SECS);
        assert!(matches!(
            code::read_initiator(&text, 1_000),
            Err(code::CodeError::WrongSide(_))
        ));
    }

    #[test]
    fn watch_takes_only_a_spectate_code_and_stays_in_scope() {
        let watch = |s: String, now: u64| match parse_at(&args(&s), 5, now) {
            Ok(Parsed::Watch(w)) => Ok(*w),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e),
        };
        let w = watch(
            "--watch --bind 127.0.0.1:7201 --peer 127.0.0.1:7100".to_string(),
            0,
        )
        .unwrap();
        assert_eq!(w.cfg.host, vec!["127.0.0.1:7100".parse().unwrap()]);
        assert_eq!(
            (w.cfg.session_id, w.cfg.secret, w.cfg.nonce),
            (DEFAULT_SPECTATE_SESSION, DEFAULT_SPECTATE_SECRET, 5)
        );
        assert_eq!(
            (w.cfg.start_after_ms, w.cfg.silent_end_ms, w.cfg.max_ms),
            (0, DEFAULT_SILENT_END_MS, DEFAULT_WATCH_MAX_MS)
        );
        assert_eq!(w.scope, Scope::Loopback);
        let w = watch(
            "--watch --bind 127.0.0.1:7201 --peer 127.0.0.1:7100 --session 7 \
             --secret 000102030405060708090a0b0c0d0e0f --name w --nonce 0x10 --start-after-ms 3000 \
             --silent-sec 4 --max-sec 60"
                .to_string(),
            0,
        )
        .unwrap();
        assert_eq!(
            (w.cfg.session_id, w.cfg.nonce, w.cfg.name.as_str()),
            (7, 16, "w")
        );
        assert_eq!(
            (w.cfg.start_after_ms, w.cfg.silent_end_ms, w.cfg.max_ms),
            (3_000, 4_000, 60_000)
        );
        let host = SpectateCli {
            bind: "127.0.0.1:7100".parse().unwrap(),
            session_id: 0xABCD_0001,
            secret: Secret([0x33; 16]),
            key_seed: crate::keys::fresh_seed(),
        };
        let spec_text = spectate_code(&host, "host", 1_000).unwrap();
        let w = watch(
            format!("--watch --bind 127.0.0.1:7201 --code {spec_text}"),
            1_000,
        )
        .unwrap();
        assert_eq!(
            (w.cfg.session_id, w.cfg.secret, w.cfg.host.clone()),
            (0xABCD_0001, Secret([0x33; 16]), vec![host.bind])
        );
        let mut cands = Candidates::EMPTY;
        cands
            .push(
                higanr_core::candidate::Candidate::guess("127.0.0.1:47001".parse().unwrap())
                    .unwrap(),
            )
            .unwrap();
        let play_text = code::write_initiator(&code::Code {
            side: code::Side::Initiator,
            session_id: 0xCAFE_0001,
            secret: Some(Secret([0x42; 16])),
            name: Name::new("dll"),
            nat: higanr_core::nat::NatMark::UNKNOWN,
            candidates: cands,
            expires: 1_000,
        })
        .unwrap();
        let e = watch(
            format!("--watch --bind 127.0.0.1:7201 --code {play_text}"),
            1_000,
        )
        .unwrap_err();
        assert!(e.contains("WrongSide"), "{e}");
        let e = parse_at(
            &args(&format!("--bind 127.0.0.1:7001 --code {spec_text}")),
            1,
            1_000,
        )
        .unwrap_err();
        assert!(e.contains("WrongSide"), "{e}");
        assert!(
            watch(
                format!("--watch --bind 127.0.0.1:7201 --code {spec_text}"),
                1_000 + code::DEFAULT_TTL_SECS + 1
            )
            .unwrap_err()
            .contains("Expired")
        );
        for (s, why) in [
            (
                format!("--watch --bind 127.0.0.1:7201 --code {spec_text} --peer 127.0.0.1:1"),
                "一緒に使わない",
            ),
            (
                format!("--watch --bind 127.0.0.1:7201 --code {spec_text} --session 1"),
                "一緒に使わない",
            ),
            (
                "--watch --bind 127.0.0.1:7201 --peer 192.168.1.9:7100".to_string(),
                "scope = loopback",
            ),
            (
                "--watch --bind 192.168.1.5:7201 --peer 127.0.0.1:7100".to_string(),
                "scope = loopback",
            ),
            (
                "--watch --bind 127.0.0.1:7201 --peer 127.0.0.1:0".to_string(),
                "ポートが 0",
            ),
            (
                "--watch --bind 127.0.0.1:7201 --peer 127.0.0.1:7100 --drop 5".to_string(),
                "使えない",
            ),
            (
                "--watch --bind 127.0.0.1:7201 --peer 127.0.0.1:7100 --spectate-host 127.0.0.1:1"
                    .to_string(),
                "使えない",
            ),
            (
                "--watch --bind 127.0.0.1:7201".to_string(),
                "--peer か --code",
            ),
            ("--watch --peer 127.0.0.1:7100".to_string(), "--bind が要る"),
            (
                "--watch --bind 127.0.0.1:7201 --peer 127.0.0.1:7100 --silent-sec 0".to_string(),
                "1〜3600",
            ),
            (
                "--watch --bind 127.0.0.1:7201 --peer 127.0.0.1:7100 --max-sec 0".to_string(),
                "1〜86400",
            ),
        ] {
            let e = watch(s.clone(), 0).unwrap_err();
            assert!(e.contains(why), "{s}: {e}");
        }
        assert_eq!(
            parse(&args("--compare-spectate a.csv b.log"), 1),
            Ok(Parsed::CompareSpectate(
                "a.csv".into(),
                "b.log".into(),
                None
            ))
        );
        assert_eq!(
            parse(&args("--compare-spectate a.csv b.log e.csv"), 1),
            Ok(Parsed::CompareSpectate(
                "a.csv".into(),
                "b.log".into(),
                Some("e.csv".into())
            ))
        );
        assert!(parse(&args("--compare-spectate a.csv"), 1).is_err());
    }
}
