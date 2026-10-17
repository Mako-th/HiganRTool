//! 設置の後の計測（pure）: 100 ms ごとにリングバッファから tick ごとの記録を読み、1 秒ごとにファイルへ追記し、1 秒ごとに入口を通った回数を、
//! 60 秒ごとに「始めからここまで」の集計を本ログに書く。読み取りのスレッド（ゲームのスレッドではない）で走るので、待ちもログの I/O も構わない。
//!
//! - 計測の長さは `render_only::TOTAL_SECONDS`（180 秒）。ゲームを途中で閉じても残るよう、1 秒ごとに追記する
//!   （`_ticks.csv` / `_changes.csv` / `_snap.bin`。メニューとライフの `_menu.csv` は別のスレッドが自分で追記する）。
//! - TSC の周波数は始めの 1 秒の QueryPerformanceCounter との対で決め（CSV の時刻の列はこれで流して書く）、集計のたびに
//!   「始めから今まで」の対との差を ppm で書く。
//! - 集計（`ticks::summarize`）は 60 秒ごとに始めからここまでを本ログへ追記する。
//! - メモリには全件の 1024 バイトを貯めない。持つのは集計に要る先頭（`Record`）と、次の追記までの 1 秒ぶんだけ。
//!
//! Win32 は `RecordEnv` の向こうに置く（本物は `record_win::RealRecord`）。時計・待ち・回数は `HookEnv` のもの。
#![forbid(unsafe_code)]

use std::io::Write;

use crate::copies::{
    self, CHANGES_HEADER, change_rows, change_words, csv_columns, row_text, snap_header,
};
use crate::draw::{DrawRead, DrawRecord};
use crate::feeds::FeedRead;
use crate::hook::HookEnv;
use crate::log_mode::detail_out;
use crate::menu::TickStats;
use crate::net_watch::{NetFrameRing, NetSnapshot};
use crate::render_only::{
    self, DRAWS_HEADER, DrawCsv, RENDERONLY_HEADER, burst_at, renderonly_row,
};
use crate::script_watch::ScriptWatch;
use crate::scripted::Progress;
use crate::shots::{ShotCounters, ShotHead};
use crate::th09_addr::{WINDOW_ADDR, WINDOW_CLOCK_DIVISOR};
use crate::ticks::{
    CSV_HEADER, Collected, CopyRow, RING_LEN, SlotRead, Snap, TickCsv, summarize, tsc_frequency,
    write_summary,
};
use crate::trial_summary::{self, Facts, GameFacts, HostFacts, PaceEnd, Stage};

/// 集計を本ログへ書く間隔（秒）。180 秒の実験では 60 秒・120 秒・終わりの 3 回書く。
pub const SUMMARY_EVERY: u32 = 60;
/// リングバッファを読む間隔（ms）。4096 件のリングバッファは 60 fps で約 68 秒分なので、上書きは起きない見込み。
pub const POLL_MS: u32 = 100;
pub const POLLS_PER_SECOND: u32 = 1000 / POLL_MS;

pub const TICKS_SUFFIX: &str = "_ticks.csv";
pub const CHANGES_SUFFIX: &str = "_changes.csv";
pub const SNAP_SUFFIX: &str = "_snap.bin";
pub const MENU_SUFFIX: &str = "_menu.csv";
/// 描画のリングバッファと、描画だけのフレームの件ごとの差の CSV。
pub const DRAWS_SUFFIX: &str = "_draws.csv";
pub const RENDERONLY_SUFFIX: &str = "_renderonly.csv";

/// D3D8 のデバイスの下見（読むだけ）。`[0x4B3108]`・その vtable・EndScene `[vtable+0x8C]`・Present `[vtable+0x3C]` のアドレスと、
/// EndScene / Present が入っているモジュールのパス。読めなかった欄は `None`。デバイスの関数は呼ばない。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct D3d8Probe {
    pub device: Option<u32>,
    pub vtable: Option<u32>,
    pub endscene: Option<u32>,
    pub present: Option<u32>,
    pub endscene_module: Option<String>,
    pub present_module: Option<String>,
}

impl D3d8Probe {
    fn field(addr: Option<u32>) -> String {
        addr.map_or("読めない".to_string(), |a| format!("0x{a:08X}"))
    }

    pub fn lines(&self) -> String {
        format!(
            "D3D8 の下見: デバイス [0x4B3108] = {} / vtable = {} / EndScene [vtable+0x8C] = {} / Present [vtable+0x3C] = {}\nD3D8 の下見: EndScene のモジュール = {} / Present のモジュール = {}（デバイスの関数は呼ばない。M1c-2 の材料）",
            Self::field(self.device),
            Self::field(self.vtable),
            Self::field(self.endscene),
            Self::field(self.present),
            self.endscene_module.as_deref().unwrap_or("分からない"),
            self.present_module.as_deref().unwrap_or("分からない"),
        )
    }
}

/// 観戦者のセッションの本ログの材料（`RecordEnv::watch_log`）。観戦者の実行は計測が締まらず、閉じたときに書く経路も無い（`write_link_end` の注）ので、
/// 数は途中で本ログへ残す。ルーム名・アドレス・接続コードは含まない。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchLog {
    /// 前の見回りの後に片づけた観戦の節（番号と中身）。
    pub ended: Vec<(u32, String)>,
    /// 観戦者のセッションがある間の、いまのルームの数の行と観戦者の集計。セッションが無ければ `None`。
    pub live: Option<String>,
}

/// 観戦の節と途中経過を本ログへ書く。片づけた節は取り出した次の見回りで 1 回だけ、途中経過は `interim`（`SUMMARY_EVERY` の秒）のときだけ。
/// セッションが無い実行は何も書かない。
fn write_watch_log(out: &mut dyn Write, k: u32, log: &WatchLog, interim: bool) {
    for (n, body) in &log.ended {
        let _ = writeln!(
            out,
            "==== {n} 回目の観戦の終わり（計測 {k} 秒の時点。計測は続ける） ===="
        );
        // 簡潔には、観戦の終わりの理由とルームの数の行を残し、受信の数の dump（`受信 …`）は詳細だけにする。
        for l in body.lines() {
            if l.starts_with("受信 ") {
                let _ = writeln!(detail_out(out), "{l}");
            } else {
                let _ = writeln!(out, "{l}");
            }
        }
    }
    if let (true, Some(live)) = (interim, &log.live) {
        let _ = writeln!(
            detail_out(out),
            "---- 観戦の途中経過（計測 {k} 秒の時点までの合計） ----\n{live}"
        );
    }
}

pub trait RecordEnv {
    fn next_seq(&self) -> u32;
    /// 番号の枠を札・中身・札の順に読んだ結果。
    fn read_slot(&self, seq: u32) -> SlotRead;
    /// TSC と QueryPerformanceCounter を続けて読んだ対（TSC は QPC の前後で 2 回読んだ中点）。
    fn tsc_qpc(&mut self) -> (u64, i64);
    fn qpc_frequency(&self) -> i64;
    /// CPUID の invariant TSC（`0x80000007` の EDX bit 8）。その葉が無ければ `None`。
    fn invariant_tsc(&self) -> Option<bool>;
    /// ゲームの時計の分母 `[0x4B30C4]`（ウィンドウ `+0x14`）。読めなければ `None`。
    fn game_clock_divisor(&mut self) -> Option<u32>;
    fn copy_table(&self) -> Vec<CopyRow>;
    /// 本ログの隣に、本ログの名前の先頭 ＋ `suffix` のファイルを開く（回ごとに別名。既存を上書きしない）。
    /// 表示用の名前と書き込み先。
    fn open_output(&mut self, suffix: &str) -> Result<(String, Box<dyn Write>), String>;
    /// メニューとライフの読み取りのスレッドを起こす（`_menu.csv` に自分で追記する）。表示用のファイル名。
    fn start_menu(&mut self) -> Result<String, String>;
    fn menu_stats(&mut self) -> Option<TickStats>;
    fn stop_menu(&mut self) -> Option<TickStats>;

    // ---- 描画のリングバッファ・描画だけのフレーム・D3D8 の下見。既定は「何もしない」。
    fn draw_next_seq(&self) -> u32 {
        0
    }
    fn read_draw_slot(&self, _seq: u32) -> DrawRead {
        DrawRead::NotYet
    }
    fn draw_calls(&self) -> u32 {
        0
    }
    /// 描画だけのフレームを `n` 回作るよう入口に頼む（残りが 0 のときだけ置ける）。置けたら true。既定は「頼めない」。
    fn start_render_only(&mut self, _n: u32) -> bool {
        false
    }
    /// いま入口に頼んである、残りの描画だけのフレームの数。
    fn render_only_left(&self) -> u32 {
        0
    }
    fn d3d8_probe(&mut self) -> D3d8Probe {
        D3d8Probe::default()
    }
    /// 文字の描画が止まった理由の本ログの行。止めたあと最初に呼んだ 1 回だけ返す（ゲームのスレッドは原子の値を置くだけで、書くのはこの側）。
    fn overlay_stop_line(&mut self) -> Option<String> {
        None
    }

    // ---- 台本の feed のリングバッファ・進み具合・撮影の置き場。既定は「台本なし」で何も返さない。
    fn feed_next_seq(&self) -> u32 {
        0
    }
    fn read_feed(&self, _seq: u32) -> FeedRead {
        FeedRead::NotYet
    }
    fn script_progress(&self) -> Progress {
        Progress::default()
    }
    /// 「できた」撮影の置き場（撮った順で最も古いもの）の番号と先頭。
    fn take_shot(&mut self) -> Option<(usize, ShotHead)> {
        None
    }
    /// 「できた」置き場の画素の先頭 `len` バイトのコピー。
    fn copy_shot(&self, _slot: usize, _len: usize) -> Vec<u8> {
        Vec::new()
    }
    fn release_shot(&mut self, _slot: usize) {}
    fn shot_counters(&self) -> ShotCounters {
        ShotCounters::default()
    }

    // ---- ネットワークの使ったフレームのリングバッファと数。既定は「ネットワークなし」。
    fn net_frames(&self) -> Option<&NetFrameRing> {
        None
    }
    /// ネットワークの数と、ネットワークの外の数（ログ用の文。受信スレッド・開始のフック）。
    fn net_status(&self) -> Option<(NetSnapshot, String)> {
        None
    }
    /// 観戦者のセッションの本ログの材料（1 秒に 1 回）。`ended` は取り出したら空になる（1 つの節は 1 回だけ返る）。観戦者のセッションが無い実行は何も返さない（既定）。
    fn watch_log(&mut self) -> WatchLog {
        WatchLog::default()
    }
    /// 試験のまとめの材料のうち、ゲームの中の静的な値から引くもの（ネットワークを開いたときのこと・接続コードの見張りの状態・パニック・自動保存）。
    /// 既定は「何も無い」。
    fn host_facts(&mut self) -> HostFacts {
        HostFacts::default()
    }
}

/// 試験のまとめの材料を集める。`net` はその回のネットワークの数（ネットワークのセッションを置いていなければ None）。
pub fn trial_facts(
    game: &GameFacts,
    rec: &mut dyn RecordEnv,
    net: Option<NetSnapshot>,
    pace_end: Option<PaceEnd>,
    stage: Stage,
) -> Facts {
    Facts {
        game: game.clone(),
        net,
        host: rec.host_facts(),
        pace_end,
        stage,
    }
}

/// 巻き戻しの要約の行の先頭（`rollback_play::summary_lines` / `hold_lines` / `core_stats_lines` と `script_win::rollback_text` の行はどれもこれで始まる）。
const ROLLBACK_LINE_HEAD: &str = "巻き戻し（";

/// 途中経過の巻き戻しの要約。`net_text` は `RecordEnv::net_status` の文で、終わりの要約と同じ行をそのまま選んで書く。
/// 巻き戻しの行が無い実行（delay 方式・rollback の入口を書いていない）は何も書かない。
/// 前の途中経過と同じ行（数が 1 つも変わらない。マッチの間やメニューにいる間）は書かない。
fn write_rollback_interim(out: &mut dyn Write, k: u32, net_text: &str, last: &mut Option<String>) {
    let body = rollback_body(net_text);
    if last.as_deref() == Some(body.as_str()) {
        return;
    }
    write_rollback_lines(&mut detail_out(out), "途中経過", k, net_text);
    *last = Some(body);
}

/// 巻き戻しの要約の行だけを改行でつないだもの（途中経過が前と同じかを比べる）。
fn rollback_body(net_text: &str) -> String {
    net_text
        .lines()
        .filter(|l| l.starts_with(ROLLBACK_LINE_HEAD))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 巻き戻しの要約の見出しと行（途中経過と、つながりの終わりの節が使う）。`what` は見出しの括弧の先頭。
fn write_rollback_lines(out: &mut dyn Write, what: &str, k: u32, net_text: &str) {
    let mut lines = net_text
        .lines()
        .filter(|l| l.starts_with(ROLLBACK_LINE_HEAD))
        .peekable();
    if lines.peek().is_none() {
        return;
    }
    let _ = writeln!(
        out,
        "---- 巻き戻しの要約（{what}。計測 {k} 秒の時点までの合計。終わりの要約と同じ形） ----"
    );
    for l in lines {
        let _ = writeln!(out, "{l}");
    }
}

/// つなぎ直せる実行（ゲーム内のメニューの実行。`NetSnapshot::relink`）で、つながりが 1 本終わった時点の「終わり」相当のまとめ
/// （つながりが終わったのを初めて見た秒に 1 回）。この実行の計測はゲームを閉じるまで締まらず、閉じたときに書く経路も無い（`DllMain` は ATTACH だけ）
/// ので、つながりごとに同じ中身をここで書く。ネットワークが終わってから 10 秒待たずに見た秒で書くのは、人が次のつながりを開くとネットワークの数が 0 から
/// 置き直され（`NetStatus::reset`）、待つと次のつながりの数を書いてしまうため。試験のまとめは終わりの節（`Stage::Final`）で判じ、
/// 見出しだけ差し替える（終わりの節の見出しは計測の終わりの 1 回だけにしておく。検査がその見出しで終わりの節を探す）。
#[allow(clippy::too_many_arguments)] // 終わりの段階と同じ材料を 1 か所で渡す
fn write_link_end(
    out: &mut dyn Write,
    n: u32,
    k: u32,
    game: &GameFacts,
    rec: &mut dyn RecordEnv,
    snap: NetSnapshot,
    net_text: &str,
    col: &Collected,
    freq0: Option<f64>,
    start_pair: (u64, i64),
    qpf: i64,
    streams: &Streams,
    link_log: &crate::link_log::LinkLog,
) {
    let what = format!("{n} 回目のつながりの終わり");
    let _ = writeln!(
        out,
        "==== {what}（計測 {k} 秒の時点。計測は続ける。次のつながりを待つ） ===="
    );
    let pair = rec.tsc_qpc();
    let menu = rec.menu_stats();
    write_progress(
        &mut detail_out(out),
        &what,
        col,
        freq0,
        start_pair,
        pair,
        qpf,
        streams,
        menu,
        None,
    );
    let facts = trial_facts(game, rec, Some(snap), None, Stage::Final);
    let text = trial_summary::render(&facts);
    // 見出しの行（`render` の 1 行目）を落として、この節の見出しに替える。
    let body = text.split_once('\n').map_or("", |(_, b)| b);
    let _ = write!(
        out,
        "==== 試験のまとめ（{what}。計測 {k} 秒の時点） ====\n{body}"
    );
    for l in link_log.end_lines(&snap) {
        let _ = writeln!(out, "{l}");
    }
    write_rollback_lines(&mut detail_out(out), &what, k, net_text);
}

/// 台本が終わるか止まってから計測を終えるまで（秒）。台本のある実行の上限は 1200 秒（10 分の台本は、マッチまでの段階と読み込みを足すと
/// 600 秒を越える。上限に着くと読み取りのスレッドが先に終わり、`_net_frames.csv` の終わりが欠ける）。台本の無いネットワークの実行（人が遊ぶ実行）は
/// この上限を使わない（`RunLen::Net` の `max` が `None`）。
pub const SCRIPT_TAIL_SECONDS: u32 = 10;
pub const SCRIPT_MAX_SECONDS: u32 = 1200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunLen {
    /// 決まった秒数（台本なしの実行。本物は `render_only::TOTAL_SECONDS`）。描画だけのフレームの予定を作る。
    Fixed(u32),
    /// 台本のある実行: 台本が終わるか止まってから `tail` 秒で終える（上限 `max` 秒）。描画だけのフレームは作らない。
    /// `steps` は台本の段階の数（ログ用）、`feed_first` は台本を動かす前の feed のリングバッファの次の番号（そこから読む）。
    Script {
        tail: u32,
        max: u32,
        steps: usize,
        feed_first: u32,
    },
    /// ネットワークの実行（台本の無いネットワーク、か `after_script = keep` の台本のネットワーク）: 台本が終わり、かつネットワークが終わってから（ネットワークのセッションを置いていなければ
    /// 終わっているとみる）`tail` 秒で終える（上限 `max` 秒）。描画だけのフレームは作らない。欄の意味は `Script` と同じ。
    /// `max` が `None` なのは台本の無いネットワークの実行（人が遊ぶ実行）だけで、ネットワークが開いている間は上限で止めない
    /// （約 26 分の 7 マッチで上限 1200 秒に着き、後半のマッチの記録が残らなかった）。
    Net {
        tail: u32,
        max: Option<u32>,
        steps: usize,
        feed_first: u32,
    },
}

/// この実行で描画だけのフレームを作るか。作るのは台本なしの実行（`Fixed`）だけで、配るバージョン（feature `dist`）ではそれも作らない
/// （ネットワークなしで起こした配布版の花映塚が 3 秒ごとに止まって見えるため）。`dist` を引数で受けるのは、配るバージョンの形を開発用のビルドの検査でも見るため。
fn makes_render_only(len: RunLen, dist: bool) -> bool {
    matches!(len, RunLen::Fixed(_)) && !dist
}

/// リングバッファから新しい件を読む。`next` は次に読む番号で、読んだ分だけ進める。採った件は、先頭を `c` に、枠の全部を
/// `batch`（次の追記まで持つ）に入れる。
/// - 一周以上遅れていたら（入口が `next` から `RING_LEN` を越えて先まで書いた）、もう上書きされた分を
///   読まずに飛ばし、読み落としに数える。
/// - 札が大きい（読む前に上書きされた）件は読み落としに数えて進む。
/// - まだ書き終わっていない件で止め、次の読みでその件からやり直す。
pub fn drain(rec: &dyn RecordEnv, next: &mut u32, c: &mut Collected, batch: &mut Vec<Snap>) {
    let end = rec.next_seq();
    let behind = end.wrapping_sub(*next);
    if behind > RING_LEN {
        let skip = behind - RING_LEN;
        c.lost += skip as u64;
        *next = next.wrapping_add(skip);
    }
    while *next != end {
        match rec.read_slot(*next) {
            SlotRead::Record(s) => {
                c.push(s.rec);
                batch.push(s);
            }
            SlotRead::Overwritten => c.lost += 1,
            SlotRead::NotYet => break,
        }
        *next = next.wrapping_add(1);
    }
}

/// 描画のリングバッファから新しい件を読む（`drain` の描画バージョン。読み落としの数え方も同じ）。採った件は `batch` に入れ、
/// 番号ごとの数えは呼び出し側が `render_only::Collector::feed_draw` でする。
pub fn drain_draws(
    rec: &dyn RecordEnv,
    next: &mut u32,
    lost: &mut u64,
    batch: &mut Vec<DrawRecord>,
) {
    let end = rec.draw_next_seq();
    let behind = end.wrapping_sub(*next);
    if behind > RING_LEN {
        let skip = behind - RING_LEN;
        *lost += skip as u64;
        *next = next.wrapping_add(skip);
    }
    while *next != end {
        match rec.read_draw_slot(*next) {
            DrawRead::Record(r) => batch.push(r),
            DrawRead::Overwritten => *lost += 1,
            DrawRead::NotYet => break,
        }
        *next = next.wrapping_add(1);
    }
}

/// 1 秒ごとの回数の集計。増分は「前に読んだときからの増え方」で、1 回の間隔はおよそ 1000 ms（`GetTickCount64` の刻みは約 16 ms）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObserveSummary {
    pub samples: u32,
    /// 最初に読んだ値から最後に読んだ値までの増え方。
    pub total: u32,
    pub min_step: u32,
    pub max_step: u32,
}

fn yes_no(b: Option<bool>) -> &'static str {
    match b {
        Some(true) => "あり",
        Some(false) => "なし",
        None => "分からない（CPUID に 0x80000007 の葉が無い）",
    }
}

/// 追記の書き込み先 1 つ。開けない・書けなくなったら理由を持ち、以後は書かない（計測は続ける）。
pub(crate) struct Sink {
    name: String,
    w: Option<Box<dyn Write>>,
    /// 書いた行（`_snap.bin` は件）の数。見出しは数えない。
    count: u64,
    err: Option<String>,
    /// 初めから開かない書き込み先（配るバージョンの観測用のファイル。`Sink::off`）。
    off: bool,
}

impl Sink {
    /// 開かない書き込み先。ファイルを作らず、書いても何もしない（`write` は false）。
    pub(crate) fn off(suffix: &str) -> Sink {
        Sink {
            name: suffix.to_string(),
            w: None,
            count: 0,
            err: None,
            off: true,
        }
    }

    /// 書く先があるか（開けなかった・書けなくなった・開かない書き込み先なら false）。行を組む手間を省くのに使う。
    pub(crate) fn is_on(&self) -> bool {
        self.w.is_some()
    }

    pub(crate) fn open(rec: &mut dyn RecordEnv, suffix: &str) -> Sink {
        match rec.open_output(suffix) {
            Ok((name, w)) => Sink {
                name,
                w: Some(w),
                count: 0,
                err: None,
                off: false,
            },
            Err(why) => Sink {
                name: suffix.to_string(),
                w: None,
                count: 0,
                err: Some(format!("開けない（{why}）")),
                off: false,
            },
        }
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> bool {
        let Some(w) = self.w.as_mut() else {
            return false;
        };
        match w.write_all(bytes) {
            Ok(()) => true,
            Err(e) => {
                self.err = Some(format!("書けなくなった（{e}）"));
                self.w = None;
                false
            }
        }
    }

    pub(crate) fn line(&mut self, s: &str) {
        if self.write(s.as_bytes()) && self.write(b"\n") {
            self.count += 1;
        }
    }

    pub(crate) fn flush(&mut self) {
        if let Some(w) = self.w.as_mut()
            && let Err(e) = w.flush()
        {
            self.err = Some(format!("書けなくなった（{e}）"));
            self.w = None;
        }
    }

    pub(crate) fn status(&self, unit: &str) -> String {
        if self.off {
            return format!("{} は書かない（配布バージョン）", self.name);
        }
        match &self.err {
            None => format!("{} に {} {unit}", self.name, self.count),
            Some(e) => format!("{} は {e}（それまでに {} {unit}）", self.name, self.count),
        }
    }
}

/// 追記のファイルと、行を流して作る状態。描画だけのフレームの差 `_renderonly.csv` は最後にまとめて書くので、集める `collector` だけ持つ。
struct Streams {
    rows: Vec<CopyRow>,
    csv: TickCsv,
    prev_words: Option<Vec<(u32, Option<u32>)>>,
    ticks: Sink,
    changes: Sink,
    snap: Sink,
    draws: Sink,
    draw_csv: DrawCsv,
    /// 描画だけのフレームと次の件・描画の番号ごとの数え（最後に `_renderonly.csv` と集計へ）。
    collector: render_only::Collector,
    draw_lost: u64,
    /// `_snap.bin` に書く件の上限（製品は `SNAP_MAX_RECORDS`。検査は小さくする）。
    snap_cap: u64,
    /// `_snap.bin` の上限を越えて書かなかった件。
    snap_skipped: u64,
}

/// `_snap.bin` に書く件の上限（計測の上限 `SCRIPT_MAX_SECONDS` × 60 tick。約 70 MB）。
const SNAP_MAX_RECORDS: u64 = SCRIPT_MAX_SECONDS as u64 * 60;

/// 観測用の 4 つ（`_ticks.csv`・`_changes.csv`・`_snap.bin`・`_draws.csv`）を書くか。配るバージョンでは書かない（配った版を使う人の 1 回の起動のログの約 88% がこの 4 つだった）。
/// 集計（本ログ）・`_net_frames.csv`・`_pacing.csv`・`_menu.csv` は、配るバージョンでも詳細なログ（ini の `[log] detail = 1`）なら書く。
fn writes_observe_files(dist: bool) -> bool {
    !dist
}

/// 台本の実行の 2 つ（`_feed.csv`・`_shots.csv`）を作るか。配るバージョンでは作らない（台本の実行でしか中身が入らず、見出しだけの空のファイルになる）。
fn writes_script_files(dist: bool) -> bool {
    !dist
}

/// 途中経過の 1 秒ごとの判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InterimDue {
    /// 試験のまとめの途中経過の節を書くか。
    trial: bool,
    /// 巻き戻しの要約の途中経過の節を書くか。
    rollback: bool,
}

/// 途中経過を書くか。試験のまとめは、RTT 以外の印の並びが変わったときと `SUMMARY_EVERY` 秒ごと。RTT の印は rollback で片道が D の時間を
/// またいで 〇 ⇄ － と揺れる（1 起動で印の変わり 2,319 回のうち 2,240 回）ので、比べから外す。巻き戻しの要約は累積なので
/// `SUMMARY_EVERY` 秒ごとだけ（1 節が約 28 KB）。
fn interim_due(
    last: Option<&[trial_summary::Mark]>,
    now: &[trial_summary::Mark],
    k: u32,
) -> InterimDue {
    let every = k.is_multiple_of(SUMMARY_EVERY);
    InterimDue {
        trial: last != Some(now) || every,
        rollback: every,
    }
}

impl Streams {
    fn open(rec: &mut dyn RecordEnv, rows: Vec<CopyRow>, files: bool) -> Streams {
        let open = |rec: &mut dyn RecordEnv, suffix: &str| {
            if files {
                Sink::open(rec, suffix)
            } else {
                Sink::off(suffix)
            }
        };
        let mut ticks = open(rec, TICKS_SUFFIX);
        let mut changes = open(rec, CHANGES_SUFFIX);
        let mut snap = open(rec, SNAP_SUFFIX);
        let mut draws = open(rec, DRAWS_SUFFIX);
        ticks.write(format!("{CSV_HEADER},{}\n", copies::csv_header()).as_bytes());
        changes.write(format!("{CHANGES_HEADER}\n").as_bytes());
        snap.write(&snap_header(&rows));
        draws.write(format!("{DRAWS_HEADER}\n").as_bytes());
        Streams {
            rows,
            csv: TickCsv::new(None),
            prev_words: None,
            ticks,
            changes,
            snap,
            draws,
            draw_csv: DrawCsv::new(None),
            collector: render_only::Collector::default(),
            draw_lost: 0,
            snap_skipped: 0,
            snap_cap: SNAP_MAX_RECORDS,
        }
    }

    fn set_freq(&mut self, freq: Option<f64>) {
        self.csv.freq_hz = freq;
        self.draw_csv.freq_hz = freq;
    }

    fn write_batch(&mut self, batch: &[Snap], draws: &[DrawRecord]) {
        for s in batch {
            // 書く先が無いときは行を組まない（配るバージョン。集めの `collector` には下で食わせる）。
            if self.ticks.is_on() {
                let line = format!("{},{}", self.csv.row(&s.rec), csv_columns(&self.rows, s));
                self.ticks.line(&line);
            }
            if self.changes.is_on() {
                let words = change_words(&self.rows, s);
                for l in change_rows(self.prev_words.as_deref(), &words, s) {
                    self.changes.line(&l);
                }
                self.prev_words = Some(words);
            }
            // コピーの生のバイト（1 件 1024 B、約 61 KB/s）は計測の上限と同じ長さぶんで書くのをやめる。台本の無いネットワークの実行は計測が上限を越えて
            // 続くので、止めないと 1 時間で約 221 MB になる。
            if self.snap.is_on() {
                if self.snap.count < self.snap_cap {
                    if self.snap.write(&s.bytes()) {
                        self.snap.count += 1;
                    }
                } else {
                    self.snap_skipped += 1;
                }
            }
            self.collector.feed_tick(s);
        }
        for r in draws {
            if self.draws.is_on() {
                let line = self.draw_csv.row(r);
                self.draws.line(&line);
            }
            self.collector.feed_draw(r);
        }
        self.ticks.flush();
        self.changes.flush();
        self.snap.flush();
        self.draws.flush();
    }

    #[cfg(test)]
    fn write_status(&self, out: &mut dyn Write) {
        let _ = writeln!(out, "{}", self.status_text());
    }

    fn status_text(&self) -> String {
        format!(
            "書き出し: {} / {} / {}{} / {}（描画の読み落とし {}）",
            self.ticks.status("行"),
            self.changes.status("行"),
            self.snap.status("件（1024 バイトずつ）"),
            if self.snap_skipped > 0 {
                format!(
                    "（上限 {} 件を越えた {} 件は書いていない）",
                    self.snap_cap, self.snap_skipped
                )
            } else {
                String::new()
            },
            self.draws.status("行"),
            self.draw_lost,
        )
    }
}

/// 途中経過の集計で、前に書いた節と変わらない行を繰り返さないための覚え。tick の集計は取れた件が上限（`MAX_RECORDS`）に達すると
/// 中身が変わらなくなり（1 時間 13 分で 262,144 件。以後 60 秒ごとに同じ行が並んだ）、書き出しの行は書かない設定だと毎回同じになる。
/// 持ちきれなかった件数のように増え続ける値は、比べから外して終わりの節に書く。終わりの集計とつながりの終わりの節は覚えを使わず全部書く。
#[derive(Default)]
struct ProgressEcho {
    ticks: Option<crate::ticks::TickSummary>,
    status: Option<String>,
    /// 省いたことを言う行を書いたか（1 本の計測で 1 回）。
    noted: bool,
}

/// `overflow`（持ちきれなかった件数。上限に達した後は増え続ける）以外が同じか。
fn same_but_overflow(a: &crate::ticks::TickSummary, b: &crate::ticks::TickSummary) -> bool {
    let mut a = a.clone();
    a.overflow = b.overflow;
    a == *b
}

#[allow(clippy::too_many_arguments)] // 集計に要るものを 1 か所で渡す
fn write_progress(
    out: &mut dyn Write,
    label: &str,
    col: &Collected,
    freq0: Option<f64>,
    start_pair: (u64, i64),
    now_pair: (u64, i64),
    qpf: i64,
    streams: &Streams,
    menu: Option<TickStats>,
    echo: Option<&mut ProgressEcho>,
) -> crate::ticks::TickSummary {
    let _ = writeln!(out, "==== 集計（{label}。始めからここまで） ====");
    let now_freq = tsc_frequency(start_pair, now_pair, qpf);
    let _ = match (freq0, now_freq) {
        (Some(f0), Some(f)) => writeln!(
            out,
            "TSC の周波数: 始めの 1 秒の対から {:.3} MHz（CSV の時刻と集計の間隔はこれで出す）/ 始めから今までの対から {:.3} MHz（ずれ {:+.1} ppm。TSC の差 {} / QPC の差 {}）",
            f0 / 1e6,
            f / 1e6,
            (f / f0 - 1.0) * 1e6,
            now_pair.0.wrapping_sub(start_pair.0),
            now_pair.1.wrapping_sub(start_pair.1)
        ),
        _ => writeln!(
            out,
            "TSC の周波数: 始めの 1 秒の対から {} / 始めから今までの対から {}（始め {start_pair:?} / 今 {now_pair:?} / QPC の周波数 {qpf}）。求まらなければ時刻の列と間隔は出さない",
            freq0.map_or("求まらない".to_string(), |f| format!(
                "{:.3} MHz",
                f / 1e6
            )),
            now_freq.map_or("求まらない".to_string(), |f| format!(
                "{:.3} MHz",
                f / 1e6
            )),
        ),
    };
    let ticks = summarize(col, freq0);
    let status = streams.status_text();
    let (same_ticks, same_status) = echo.as_deref().map_or((false, false), |e| {
        (
            e.ticks
                .as_ref()
                .is_some_and(|p| same_but_overflow(p, &ticks)),
            e.status.as_deref() == Some(status.as_str()),
        )
    });
    if !same_ticks {
        write_summary(out, &ticks);
    }
    if !same_status {
        let _ = writeln!(out, "{status}");
    }
    if let Some(e) = echo {
        if (same_ticks || same_status) && !e.noted {
            e.noted = true;
            let _ = writeln!(
                out,
                "集計: tick の集計と書き出しの行は、前の節から変わらないときは書かない（持ちきれなかった件数など増え続ける値は終わりの節に書く）"
            );
        }
        if !same_ticks {
            e.ticks = Some(ticks.clone());
        }
        e.status = Some(status);
    }
    match menu {
        Some(m) => m.write_summary(out, label),
        None => {
            let _ = writeln!(out, "メニューとライフ（{label}）: 読み取りのスレッドが無い");
        }
    }
    ticks
}

fn write_render_only(
    out: &mut dyn Write,
    rec: &mut dyn RecordEnv,
    streams: &mut Streams,
    freq0: Option<f64>,
) {
    streams.collector.finish();
    let (frames, summary) = render_only::analyze(&streams.collector.frames, &streams.rows, |seq| {
        streams.collector.draws_for(seq)
    });
    // 簡潔なログでは `_renderonly.csv` を作らない。
    let mut ro = if crate::log_mode::detail() {
        Sink::open(rec, RENDERONLY_SUFFIX)
    } else {
        Sink::off(RENDERONLY_SUFFIX)
    };
    ro.write(format!("{RENDERONLY_HEADER}\n").as_bytes());
    for f in &frames {
        let line = renderonly_row(f, freq0);
        ro.line(&line);
    }
    ro.flush();
    render_only::write_summary(out, &summary, rec.draw_calls(), rec.draw_next_seq());
    let _ = writeln!(out, "描画だけのフレーム: 書き出し {}", ro.status("行"));
}

/// 設置の後の計測。`first_seq` は設置の直前に読んだ次の番号（そこから読む）。`seconds` は計測の長さ（検査では短くする）。
/// 台本の無い実行（`RunLen::Fixed`）。
#[cfg(test)]
pub fn run(
    hook: &mut dyn HookEnv,
    rec: &mut dyn RecordEnv,
    first_seq: u32,
    out: &mut dyn Write,
    seconds: u32,
) -> (ObserveSummary, crate::ticks::TickSummary) {
    run_len(
        hook,
        rec,
        first_seq,
        out,
        RunLen::Fixed(seconds),
        &GameFacts::default(),
    )
}

/// 設置の後の計測（長さは `len`）。台本のある実行（`RunLen::Script`）では、100 ms ごとに feed のリングバッファと撮影の置き場も読み
/// （`script_watch::ScriptWatch`）、描画だけのフレームの予定は作らず、台本が終わるか止まってから `tail` 秒で終える（上限 `max` 秒）。
///
/// 試験のまとめ（`trial_summary`）: 1 秒ごとに材料を集め、項目の印の並びが変わったとき（と、60 秒ごと）に「途中経過」の節を本ログへ書く。
/// 使う人がゲームを閉じて終えると、この関数が終わりの節を書く前にプロセスごと消えるため。終わりの節は呼び手（`session::run`）が、
/// ペースなどの終わりの集計の後（本ログの最後）に書く。`game` は設置までに分かったこと。
pub fn run_len(
    hook: &mut dyn HookEnv,
    rec: &mut dyn RecordEnv,
    first_seq: u32,
    out: &mut dyn Write,
    len: RunLen,
    game: &GameFacts,
) -> (ObserveSummary, crate::ticks::TickSummary) {
    let _ = writeln!(
        detail_out(out),
        "TSC: invariant TSC（CPUID 0x80000007 の EDX bit 8）= {}",
        yes_no(rec.invariant_tsc())
    );
    let qpf = rec.qpc_frequency();
    let divisor_addr = WINDOW_ADDR + WINDOW_CLOCK_DIVISOR;
    let _ = match rec.game_clock_divisor() {
        Some(d) => writeln!(
            detail_out(out),
            "時計: プロセスの QueryPerformanceFrequency = {qpf} / ゲームの [0x{divisor_addr:08X}]（ウィンドウ +0x{WINDOW_CLOCK_DIVISOR:X}。時計の分母。0 なら timeGetTime の経路）= {d}（QPC の周波数の下位 32 ビットと{}）",
            if d == qpf as u32 {
                "一致"
            } else {
                "不一致"
            }
        ),
        None => writeln!(
            detail_out(out),
            "時計: プロセスの QueryPerformanceFrequency = {qpf} / ゲームの [0x{divisor_addr:08X}] は読めない"
        ),
    };

    let rows = rec.copy_table();
    if rows.is_empty() {
        let _ = writeln!(
            out,
            "★★ コピーの表: 入口は 0 行（何も写していない）。_ticks.csv のコピーの列と _changes.csv は空になる ★★"
        );
    } else {
        let _ = writeln!(
            detail_out(out),
            "コピーの表（入口が使っているもの。読み返し）: {} 行、合計 {} バイト: {}",
            rows.len(),
            rows.iter().map(|r| r.len).sum::<u32>(),
            rows.iter().map(row_text).collect::<Vec<_>>().join(" / ")
        );
    }
    let mut streams = Streams::open(
        rec,
        rows,
        writes_observe_files(cfg!(feature = "dist")) && crate::log_mode::detail(),
    );
    let _ = writeln!(detail_out(out), "{}", rec.d3d8_probe().lines());
    // 簡潔なログでは `_menu.csv` を書かないので、1 ms ごとに読むこのスレッドも起こさない。
    if crate::log_mode::detail() {
        let _ = match rec.start_menu() {
            Ok(name) => writeln!(
                detail_out(out),
                "メニューとライフ: 別のスレッドで約 1 ms ごとに読み、変わったときだけ {name} へ追記する"
            ),
            Err(why) => writeln!(
                out,
                "メニューとライフ: 読み取りのスレッドを起こせない（{why}）"
            ),
        };
    }
    // 台本のある実行だけ、feed のリングバッファと撮影の置き場を読む処理（`_feed.csv` / `_shots.csv`）。
    // 計測の上限（秒）。None は台本の無いネットワークの実行で、ネットワークが終わるまで回す（`RunLen::Net` の注）。
    let max_seconds: Option<u32> = match len {
        RunLen::Fixed(s) => Some(s),
        RunLen::Script { max, .. } => Some(max),
        RunLen::Net { max, .. } => max,
    };
    let max_text = max_seconds.map_or("上限なし".to_string(), |m| m.to_string());
    let mut watch = match len {
        RunLen::Fixed(_) => None,
        RunLen::Script {
            steps, feed_first, ..
        }
        | RunLen::Net {
            steps, feed_first, ..
        } => Some(ScriptWatch::open(
            rec,
            steps,
            feed_first,
            writes_script_files(cfg!(feature = "dist")) && crate::log_mode::detail(),
        )),
    };

    let start_pair = rec.tsc_qpc();
    let start = hook.now_ms();
    let first = hook.counter();
    let _ = match len {
        RunLen::Fixed(seconds) => writeln!(
            detail_out(out),
            "計測: {POLL_MS} ms ごとにリングバッファから tick ごとの記録を読み（seq {first_seq} から）、1000 ms ごとに {seconds} 回、入口を通った回数を読んでファイルへ追記する。{SUMMARY_EVERY} 秒ごとに集計（始めの値 {first}）"
        ),
        RunLen::Script { tail, max, .. } => writeln!(
            detail_out(out),
            "計測: {POLL_MS} ms ごとにリングバッファから tick ごとの記録・feed の記録・撮影を読み（seq {first_seq} から）、1000 ms ごとに入口を通った回数を読んでファイルへ追記する。台本が終わるか止まってから {tail} 秒で終える（上限 {max} 秒）。{SUMMARY_EVERY} 秒ごとに集計（始めの値 {first}）"
        ),
        RunLen::Net {
            tail,
            max: Some(max),
            ..
        } => writeln!(
            detail_out(out),
            "計測: {POLL_MS} ms ごとにリングバッファから tick ごとの記録・feed の記録・撮影を読み（seq {first_seq} から）、1000 ms ごとに入口を通った回数を読んでファイルへ追記する。台本（あれば）とネットワークが終わってから {tail} 秒で終える（上限 {max} 秒。ネットワークは計測の後も続く）。{SUMMARY_EVERY} 秒ごとに集計（始めの値 {first}）"
        ),
        RunLen::Net {
            tail, max: None, ..
        } => writeln!(
            detail_out(out),
            "計測: {POLL_MS} ms ごとにリングバッファから tick ごとの記録・feed の記録・撮影を読み（seq {first_seq} から）、1000 ms ごとに入口を通った回数を読んでファイルへ追記する。ネットワークが終わってから {tail} 秒で終える（台本の無い走りなので、ネットワークが開いている間は計測の上限で止めない。ネットワークは計測の後も続く）。{SUMMARY_EVERY} 秒ごとに集計（始めの値 {first}）"
        ),
    };
    let render_only_on = makes_render_only(len, cfg!(feature = "dist"));
    let _ = match len {
        RunLen::Fixed(_) if render_only_on => writeln!(
            detail_out(out),
            "描画だけのフレームの予定: 設置の後 {} 秒は作らず（基準）、その後 3 秒ごとに N = 1, 2, 3, 10, 60 を繰り返し、{} 秒で終える",
            render_only::BASELINE_SECONDS,
            render_only::TOTAL_SECONDS
        ),
        RunLen::Fixed(_) => writeln!(
            detail_out(out),
            "描画だけのフレームの予定: 作らない（配布バージョン）"
        ),
        RunLen::Script { .. } => {
            writeln!(
                detail_out(out),
                "描画だけのフレームの予定: 作らない（台本のある走り）"
            )
        }
        RunLen::Net { .. } => {
            writeln!(
                detail_out(out),
                "描画だけのフレームの予定: 作らない（ネットワークの実行）"
            )
        }
    };
    let mut next = first_seq;
    let mut draw_next = rec.draw_next_seq();
    let mut col = Collected::default();
    let mut batch: Vec<Snap> = Vec::new();
    let mut draw_batch: Vec<DrawRecord> = Vec::new();
    let mut freq0: Option<f64> = None;
    let mut prev = first;
    let mut min_step = u32::MAX;
    let mut max_step = 0;
    let mut measured = 0;
    let mut script_ended: Option<u32> = None;
    let mut restarts_logged = 0u32;
    // CHECK のズレを本ログへ書いたか（1 回だけ。全部の欄を書く）と、最後に書いた試験のまとめの印の並び。
    let mut desync_logged = false;
    // 同期ズレでマッチを止めたことを本ログへ書いたか（止めた秒に 1 回と、Esc で戻った秒に 1 回）。
    let mut stop_logged = (false, false);
    let mut last_marks: Option<Vec<trial_summary::Mark>> = None;
    // 簡潔なログの「状態の変化」は、前に判じた項目との差で書く。マッチの始まりと終わりの 1 行ずつは `MatchWatch`。
    let mut last_items: Option<Vec<trial_summary::Item>> = None;
    // rollback のマッチをやめた行（回数が変わるたびに 1 回。簡潔なログにも残す）。
    let mut last_abort_line: Option<String> = None;
    let mut matches = crate::match_events::MatchWatch::default();
    // つながりごとの観戦者の出入り・RTT の標本（終わりの節に stall の内訳と一緒に書く）。
    let mut link_log = crate::link_log::LinkLog::default();
    let mut last_net: Option<NetSnapshot> = None;
    // 途中経過の集計・巻き戻しの要約で、前と変わらないものを繰り返さないための覚え。
    let mut echo = ProgressEcho::default();
    let mut last_rollback_interim: Option<String> = None;
    // つなぎ直せる実行で、前の秒に見たつながりが終わっていたか（終わりの節を 1 本に 1 回だけ書くための縁の検出）と、書いた本数。
    // ネットワークの数を読めなかった秒は前の値のまま（片づけの後は、次のセッションを置くまでネットワークの数が前のつながりの終わりのまま残る）。
    let mut link_end_seen = false;
    let mut links_ended = 0u32;
    // 上限の無い実行もリングバッファの回数は u32 に収める（1 秒 10 回で約 13 年。終わりは下の `last` が決める）。
    let polls = max_seconds.map_or(u32::MAX, |m| m.saturating_mul(POLLS_PER_SECOND));
    for p in 1..=polls {
        // 予定の時刻は始めの時刻からの倍数で決め、待ちの延びが次の回へ積もらないようにする。
        let due = start + p as u64 * POLL_MS as u64;
        let now = hook.now_ms();
        if due > now {
            hook.sleep_ms(u32::try_from(due - now).unwrap_or(u32::MAX));
        }
        drain(rec, &mut next, &mut col, &mut batch);
        drain_draws(rec, &mut draw_next, &mut streams.draw_lost, &mut draw_batch);
        if let Some(w) = watch.as_mut() {
            w.poll(rec, freq0);
        }
        if !p.is_multiple_of(POLLS_PER_SECOND) {
            continue;
        }
        let k = p / POLLS_PER_SECOND;
        measured = k;
        // 描画だけのフレームの予定: この秒の区切りで N を頼む（残りが 0 のときだけ置ける）。台本・ネットワークの実行と配るバージョンでは作らない。
        if let (true, Some(n)) = (render_only_on, burst_at(k)) {
            let placed = rec.start_render_only(n);
            let _ = writeln!(
                detail_out(out),
                "描画だけのフレーム: {k} 秒で N={n} を{}（残り {}）",
                if placed {
                    "頼んだ"
                } else {
                    "頼めない（前の分が残っている）"
                },
                rec.render_only_left()
            );
        }
        if k == 1 {
            let pair = rec.tsc_qpc();
            freq0 = tsc_frequency(start_pair, pair, qpf);
            streams.set_freq(freq0);
            let _ = match freq0 {
                Some(f) => writeln!(
                    detail_out(out),
                    "TSC の周波数: 始めの 1 秒の QPC との対から {:.3} MHz（TSC の差 {} / QPC の差 {} / QPC の周波数 {qpf}）",
                    f / 1e6,
                    pair.0.wrapping_sub(start_pair.0),
                    pair.1.wrapping_sub(start_pair.1)
                ),
                None => writeln!(
                    detail_out(out),
                    "TSC の周波数: 始めの 1 秒では求まらない（始め {start_pair:?} / 1 秒 {pair:?} / QPC の周波数 {qpf}）。時刻の列と間隔は出さない"
                ),
            };
        }
        streams.write_batch(&batch, &draw_batch);
        batch.clear();
        draw_batch.clear();
        if let Some(w) = watch.as_mut() {
            w.flush();
        }
        let c = hook.counter();
        // 回数は 32 ビットで回り込むので、増分は wrapping で取る。
        let step = c.wrapping_sub(prev);
        prev = c;
        min_step = min_step.min(step);
        max_step = max_step.max(step);
        let _ = writeln!(
            detail_out(out),
            "計測 {k}/{max_text}: 経過 {} ms / 回数 {c} / 増分 {step}",
            hook.now_ms().saturating_sub(start)
        );
        if let Some(l) = rec.overlay_stop_line() {
            let _ = writeln!(out, "{l}（{k} 秒の時点で見た）");
        }
        // 別のスレッドが本ログへ頼んだ行を、簡潔なログでも 1 秒以内に書く。
        let _ = out.flush();
        // ネットワークの数は 1 秒ごとに読む（作り直しの行と、ネットワークの実行の終わりの判定）。ネットワークのセッションを置いていなければ None。
        let net_now = rec.net_status();
        let net_snap = net_now.as_ref().map(|(snap, _)| *snap);
        if let Some(snap) = net_snap {
            for l in matches.step(&snap, k) {
                let _ = writeln!(out, "{l}");
            }
            for l in link_log.step(&snap, k) {
                let _ = writeln!(out, "{l}");
            }
            last_net = Some(snap);
            while restarts_logged < snap.hs_restarts {
                restarts_logged += 1;
                let _ = writeln!(
                    out,
                    "{}（{k} 秒の時点で見た）",
                    snap.restart_line(restarts_logged)
                );
            }
            if !desync_logged && let Some(l) = snap.desync_line() {
                desync_logged = true;
                let _ = writeln!(
                    out,
                    "ネットワーク: {l}（{k} 秒の時点で見た。{}）",
                    trial_summary::compare_frames_hint()
                );
            }
            // 止めた行は止めた秒に 1 回、Esc で戻った秒にもう 1 回（途中の秒は描画だけのフレームの数が増えるだけで書かない）。
            if let Some(l) = snap.stop_line() {
                let released = snap.stop & 2 != 0;
                if !stop_logged.0 || (released && !stop_logged.1) {
                    stop_logged = (true, stop_logged.1 || released);
                    let _ = writeln!(out, "ネットワーク: {l}（{k} 秒の時点で見た）");
                }
            }
        }
        if let Some((_, text)) = &net_now
            && let Some(l) = text
                .lines()
                .find(|l| l.starts_with(crate::script_win::ROLLBACK_ABORT_HEAD))
            && last_abort_line.as_deref() != Some(l)
        {
            let _ = writeln!(out, "{l}（{k} 秒の時点で見た）");
            last_abort_line = Some(l.to_string());
        }
        let last = match len {
            RunLen::Fixed(seconds) => k >= seconds,
            RunLen::Net { tail, max, .. } => {
                if script_ended.is_none()
                    && rec.script_progress().finished()
                    && net_snap.is_none_or(|s| s.ended())
                {
                    script_ended = Some(k);
                    let _ = writeln!(
                        out,
                        "ネットワーク: {k} 秒の時点で終わっていた（{}）。{tail} 秒後に計測を終える",
                        net_snap.map_or(
                            "ネットワークのセッションを置いていない".to_string(),
                            |s| s.text()
                        )
                    );
                }
                max.is_some_and(|m| k >= m)
                    || script_ended.is_some_and(|e| k >= e.saturating_add(tail))
            }
            RunLen::Script {
                tail, max, steps, ..
            } => {
                if script_ended.is_none() {
                    let pr = rec.script_progress();
                    if pr.finished() {
                        script_ended = Some(k);
                        let _ = writeln!(
                            out,
                            "台本: {k} 秒の時点で進まなくなった（{}）。{tail} 秒後に計測を終える",
                            pr.text(Some(steps))
                        );
                    }
                }
                k >= max || script_ended.is_some_and(|e| k >= e.saturating_add(tail))
            }
        };
        // つなぎ直せる実行で、つながりが 1 本終わったのを初めて見た秒（`write_link_end`）。計測は締めない。この秒で計測を終えるなら書かない
        // （すぐ後の終わりの段階が同じ中身を書く）。
        let mut link_end_written = false;
        if let Some(s) = net_snap {
            let ended_now = s.relink && s.link_ended();
            if ended_now && !link_end_seen && !last {
                links_ended += 1;
                link_end_written = true;
                let text = net_now.as_ref().map_or("", |(_, t)| t.as_str());
                write_link_end(
                    out,
                    links_ended,
                    k,
                    game,
                    rec,
                    s,
                    text,
                    &col,
                    freq0,
                    start_pair,
                    qpf,
                    &streams,
                    &link_log,
                );
            }
            link_end_seen = ended_now;
        }
        // つながりの終わりの節を書いた秒は、途中経過（集計・試験のまとめ・巻き戻しの要約）を重ねて書かない（同じ秒の同じ数が
        // 2 回並ぶだけ）。印の並びは覚えておき、次の秒からは変わったときだけ書く。
        if k.is_multiple_of(SUMMARY_EVERY) && !last && !link_end_written {
            let pair = rec.tsc_qpc();
            let menu = rec.menu_stats();
            write_progress(
                &mut detail_out(out),
                &format!("途中 {k} 秒"),
                &col,
                freq0,
                start_pair,
                pair,
                qpf,
                &streams,
                menu,
                Some(&mut echo),
            );
        }
        if !last {
            let facts = trial_facts(game, rec, net_snap, None, Stage::Interim(k));
            let items = trial_summary::judge(&facts);
            let now_marks = trial_summary::marks_without_rtt(&items);
            if link_end_written {
                last_marks = Some(now_marks);
                last_items = Some(items);
            } else {
                let due = interim_due(last_marks.as_deref(), &now_marks, k);
                if crate::log_mode::detail() {
                    if due.trial {
                        let _ = write!(out, "{}", trial_summary::render(&facts));
                    }
                } else if last_marks.as_deref() != Some(&now_marks) {
                    // 簡潔なログは印が変わったときだけ、変わった項目の行を書く（60 秒ごとの全文は書かない）。
                    let _ = write!(
                        out,
                        "{}",
                        trial_summary::render_changes(k, &items, last_items.as_deref())
                    );
                }
                // 巻き戻しの要約は `SUMMARY_EVERY` 秒ごとだけ（終わりの要約は計測の終わりにしか出ないので、ゲームを閉じると残らない。
                // 累積なので最後の節があれば足りる）。
                if due.rollback
                    && let Some((_, text)) = &net_now
                {
                    write_rollback_interim(out, k, text, &mut last_rollback_interim);
                }
                last_marks = Some(now_marks);
                last_items = Some(items);
            }
            // 観戦者のセッションの数（片づけた節と途中経過）。最後の秒は取り出さない: 終わりの全文（ネットワークの外）が片づけた節を全部書くので、
            // 取り出すと同じ行が 2 回並ぶ。
            write_watch_log(out, k, &rec.watch_log(), k.is_multiple_of(SUMMARY_EVERY));
        }
        if last {
            break;
        }
    }
    drain(rec, &mut next, &mut col, &mut batch);
    drain_draws(rec, &mut draw_next, &mut streams.draw_lost, &mut draw_batch);
    streams.write_batch(&batch, &draw_batch);
    if let Some(w) = watch.as_mut() {
        w.poll(rec, freq0);
        w.flush();
    }
    let end_pair = rec.tsc_qpc();
    let menu = rec.stop_menu();
    let ticks = write_progress(
        &mut detail_out(out),
        "終わり",
        &col,
        freq0,
        start_pair,
        end_pair,
        qpf,
        &streams,
        menu,
        None,
    );
    // つなぎ直せない実行（つながりが終わると計測も終わる）の、つながりの終わりの行。つなぎ直せる実行は終わりを見た秒に `write_link_end` が書いている。
    if let Some(s) = last_net.filter(|s| !s.relink) {
        for l in link_log.end_lines(&s) {
            let _ = writeln!(out, "{l}");
        }
    }
    // D3D8 のデバイスの下見をもう一度読む（設置の直後は主スレッドがまだデバイスを作っていないことがあるので、描画の地点を通った後の値を見る）。
    for l in rec.d3d8_probe().lines().lines() {
        let _ = writeln!(detail_out(out), "（終わりに読み直し）{l}");
    }
    // 描画だけのフレームの実験の集計（`_renderonly.csv` と本ログ）。台本のある実行では作っていないので、描画の
    // 回数だけを書く（撮影の描画の番号と突き合わせるため）。
    match watch.as_ref() {
        None => {
            write_render_only(&mut detail_out(out), rec, &mut streams, freq0);
            // 台本の無い実行でも、ゲーム内のメニューの実行は「ネットワークの外」の行（メニューの数え）を出す。
            crate::script_watch::write_outside_net(&*rec, &mut detail_out(out));
        }
        Some(w) => {
            let _ = writeln!(
                detail_out(out),
                "描画: 描画の地点を通った回数 {}、描画のリングバッファに書いた件 {}（台本のある走りなので描画だけのフレームは作っていない）",
                rec.draw_calls(),
                rec.draw_next_seq()
            );
            w.write_summary(rec, &mut detail_out(out), freq0);
        }
    }

    let summary = ObserveSummary {
        samples: measured,
        total: prev.wrapping_sub(first),
        min_step: if measured == 0 { 0 } else { min_step },
        max_step,
    };
    let _ = writeln!(
        detail_out(out),
        "計測の集計: {} 回、合計の増分 {}、1 回（約 1000 ms）あたりの増分 最小 {} / 最大 {}",
        summary.samples,
        summary.total,
        summary.min_step,
        summary.max_step
    );
    (summary, ticks)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    // 配るバージョンでは中身の `_ticks.csv` を読む検査を走らせないので、その検査だけが使う文字は開発用だけ。
    #[cfg(not(feature = "dist"))]
    use crate::copies::tests::snap_with;
    use crate::hook::tests::FakeHook;
    use crate::menu::TimerMode;
    use crate::script_watch::{FEED_SUFFIX, SHOTS_SUFFIX};
    use crate::ticks::tests::sixty_fps;
    #[cfg(not(feature = "dist"))]
    use crate::ticks::tests::{F, FRAME_TSC};
    use crate::ticks::{COPY_TABLE, Record, SLOT_WORDS};
    use std::cell::{Cell, RefCell};
    use std::collections::{HashMap, HashSet};
    use std::rc::Rc;

    #[derive(Clone, Default)]
    pub(crate) struct SharedBuf(
        pub(crate) Rc<RefCell<Vec<u8>>>,
        pub(crate) Rc<RefCell<Vec<usize>>>,
    );
    impl Write for SharedBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            let lines = self.0.borrow().iter().filter(|&&c| c == b'\n').count();
            self.1.borrow_mut().push(lines);
            Ok(())
        }
    }
    impl SharedBuf {
        pub(crate) fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).unwrap()
        }
    }

    pub(crate) fn snap_of_record(r: &Record) -> Snap {
        let mut words = Box::new([0u32; SLOT_WORDS]);
        words[..crate::ticks::BODY_WORDS].copy_from_slice(&r.to_words());
        Snap::from_words(r.seq, words)
    }

    /// 偽のリングバッファ: `records[i]` が番号 `base + i`。`next_seq` を呼ぶたびに（= 1 回の読みごとに）
    /// `per_poll` 件ずつ見えるようになる。
    pub(crate) struct FakeRecord {
        pub(crate) records: Vec<Snap>,
        pub(crate) base: u32,
        pub(crate) per_poll: u32,
        pub(crate) polls: Cell<u32>,
        /// 見えている件数を直に決める（`per_poll` より先に効く）。
        pub(crate) visible: Option<fn(u32) -> u32>,
        pub(crate) overwritten: HashSet<u32>,
        /// 1 回目だけ「まだ」と答える番号。
        pub(crate) not_yet_once: RefCell<HashSet<u32>>,
        pub(crate) reads: RefCell<Vec<u32>>,
        /// TSC と QPC の対（呼んだ回数 → 対）と、呼んだ回数。
        pub(crate) pair_fn: fn(u32) -> (u64, i64),
        pub(crate) pairs_read: u32,
        pub(crate) qpf: i64,
        pub(crate) inv: Option<bool>,
        pub(crate) divisor: Option<u32>,
        pub(crate) rows: Vec<CopyRow>,
        pub(crate) outputs: HashMap<String, SharedBuf>,
        pub(crate) open_fail: HashSet<String>,
        pub(crate) menu_fail: bool,
        pub(crate) menu_started: bool,
        pub(crate) menu_stopped: bool,
        /// `d3d8_probe` を呼んだ回数（1 回目と 2 回目で違う値を返す検査に使う）。
        pub(crate) d3d8_calls: Cell<u32>,
        /// `d3d8_calls` が指す回数ごとに返す下見（無ければ既定の `D3d8Probe::default()`）。
        pub(crate) d3d8_by_call: HashMap<u32, D3d8Probe>,
        /// ネットワークの数（`net_status` を呼んだ回数 → 数）。None ならネットワークのセッションを置いていない。
        pub(crate) net: Option<fn(u32) -> NetSnapshot>,
        /// ネットワークの外の文（`net_status` を呼んだ回数 → 文）。None なら空の文。
        pub(crate) net_text: Option<fn(u32) -> String>,
        pub(crate) net_calls: Cell<u32>,
        /// `start_render_only` で頼まれた N の列（呼ばれた順。配線の検査 `run_asks_for_render_only_frames_…`）。
        pub(crate) render_only_asks: Vec<u32>,
        /// 観戦者のセッションの材料（`watch_log` を呼んだ回数 1, 2, … → 材料）。None なら観戦者のセッションが無い実行（既定の空）。
        pub(crate) watch: Option<fn(u32) -> WatchLog>,
        pub(crate) watch_calls: u32,
        /// `overlay_stop_line` を呼んだ回数と、その何回目に返すか・返す行（1 回だけ返す本物と同じ）。
        pub(crate) overlay_stop_calls: u32,
        pub(crate) overlay_stop: Option<(u32, String)>,
        /// 試験のまとめの材料（パニックなど）。
        pub(crate) host: HostFacts,
    }

    /// 3 GHz の TSC と 10 MHz の QPC がそろって進む対（呼ぶたびに 1 秒ずつ進む）。
    fn three_ghz(i: u32) -> (u64, i64) {
        let q = i as i64 * 10_000_000;
        (1_000_000 + q as u64 * 300, q)
    }

    impl FakeRecord {
        pub(crate) fn new(records: Vec<Record>, per_poll: u32) -> Self {
            FakeRecord::with_snaps(records.iter().map(snap_of_record).collect(), per_poll)
        }

        pub(crate) fn with_snaps(records: Vec<Snap>, per_poll: u32) -> Self {
            FakeRecord {
                base: records.first().map_or(0, |r| r.rec.seq),
                records,
                per_poll,
                polls: Cell::new(0),
                visible: None,
                overwritten: HashSet::new(),
                not_yet_once: RefCell::new(HashSet::new()),
                reads: RefCell::new(Vec::new()),
                pair_fn: three_ghz,
                pairs_read: 0,
                qpf: 10_000_000,
                inv: Some(true),
                divisor: Some(10_000_000),
                rows: COPY_TABLE.to_vec(),
                outputs: HashMap::new(),
                open_fail: HashSet::new(),
                menu_fail: false,
                menu_started: false,
                menu_stopped: false,
                d3d8_calls: Cell::new(0),
                d3d8_by_call: HashMap::new(),
                net: None,
                net_text: None,
                net_calls: Cell::new(0),
                render_only_asks: Vec::new(),
                watch: None,
                watch_calls: 0,
                overlay_stop_calls: 0,
                overlay_stop: None,
                host: HostFacts::default(),
            }
        }

        // 中身を読むのは `_ticks.csv` などを前提にした検査だけ（配るバージョンでは走らない）。
        #[cfg(not(feature = "dist"))]
        pub(crate) fn output(&self, suffix: &str) -> String {
            self.outputs
                .get(suffix)
                .map(SharedBuf::text)
                .unwrap_or_default()
        }
    }

    impl RecordEnv for FakeRecord {
        fn next_seq(&self) -> u32 {
            let p = self.polls.get() + 1;
            self.polls.set(p);
            let n = match self.visible {
                Some(f) => f(p),
                None => (p * self.per_poll).min(self.records.len() as u32),
            };
            self.base.wrapping_add(n)
        }
        fn read_slot(&self, seq: u32) -> SlotRead {
            self.reads.borrow_mut().push(seq);
            if self.overwritten.contains(&seq) {
                return SlotRead::Overwritten;
            }
            if self.not_yet_once.borrow_mut().remove(&seq) {
                return SlotRead::NotYet;
            }
            SlotRead::Record(self.records[seq.wrapping_sub(self.base) as usize].clone())
        }
        fn tsc_qpc(&mut self) -> (u64, i64) {
            let p = (self.pair_fn)(self.pairs_read);
            self.pairs_read += 1;
            p
        }
        fn qpc_frequency(&self) -> i64 {
            self.qpf
        }
        fn invariant_tsc(&self) -> Option<bool> {
            self.inv
        }
        fn game_clock_divisor(&mut self) -> Option<u32> {
            self.divisor
        }
        fn copy_table(&self) -> Vec<CopyRow> {
            self.rows.clone()
        }
        fn open_output(&mut self, suffix: &str) -> Result<(String, Box<dyn Write>), String> {
            if self.open_fail.contains(suffix) {
                return Err("合成の失敗".to_string());
            }
            let buf = self.outputs.entry(suffix.to_string()).or_default().clone();
            Ok((format!("synthetic{suffix}"), Box::new(buf)))
        }
        fn start_menu(&mut self) -> Result<String, String> {
            if self.menu_fail {
                return Err("合成の失敗".to_string());
            }
            self.menu_started = true;
            Ok(format!("synthetic{MENU_SUFFIX}"))
        }
        fn menu_stats(&mut self) -> Option<TickStats> {
            self.menu_started
                .then(|| TickStats::new(TimerMode::HighResolution))
        }
        fn stop_menu(&mut self) -> Option<TickStats> {
            self.menu_stopped = true;
            self.menu_stats()
        }
        fn d3d8_probe(&mut self) -> D3d8Probe {
            let n = self.d3d8_calls.get() + 1;
            self.d3d8_calls.set(n);
            self.d3d8_by_call.get(&n).cloned().unwrap_or_default()
        }
        fn net_status(&self) -> Option<(NetSnapshot, String)> {
            let f = self.net?;
            let n = self.net_calls.get();
            self.net_calls.set(n + 1);
            Some((f(n), self.net_text.map_or_else(String::new, |t| t(n))))
        }
        fn host_facts(&mut self) -> HostFacts {
            self.host.clone()
        }
        fn overlay_stop_line(&mut self) -> Option<String> {
            self.overlay_stop_calls += 1;
            match &self.overlay_stop {
                Some((at, line)) if *at == self.overlay_stop_calls => Some(line.clone()),
                _ => None,
            }
        }
        fn watch_log(&mut self) -> WatchLog {
            self.watch_calls += 1;
            self.watch
                .map_or_else(WatchLog::default, |f| f(self.watch_calls))
        }
        /// 頼まれた N を覚えておく（置けたかは「頼めない」のまま。既存の検査の本ログの文を変えない）。
        fn start_render_only(&mut self, n: u32) -> bool {
            self.render_only_asks.push(n);
            false
        }
    }

    #[test]
    fn snap_bin_stops_at_its_cap_but_ticks_keep_going() {
        assert_eq!(SNAP_MAX_RECORDS, u64::from(SCRIPT_MAX_SECONDS) * 60);
        let mut rec = FakeRecord::new(Vec::new(), 0);
        let rows = rec.copy_table();
        let mut s = Streams::open(&mut rec, rows, true);
        assert_eq!(s.snap_cap, SNAP_MAX_RECORDS, "製品の上限");
        s.snap_cap = 5;
        let batch: Vec<Snap> = (0..8u32)
            .map(|i| Snap::from_words(i, Box::new([0u32; SLOT_WORDS])))
            .collect();
        s.write_batch(&batch, &[]);
        assert_eq!((s.snap.count, s.snap_skipped), (5, 3));
        assert_eq!(s.ticks.count, 8, "表の行は上限に関係なく書く");
        let mut out = Vec::new();
        s.write_status(&mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("上限 5 件を越えた 3 件は書いていない"),
            "{text}"
        );
    }

    #[test]
    fn the_dist_build_does_not_open_the_four_observe_files() {
        assert!(writes_observe_files(false), "開発用は書く");
        assert!(!writes_observe_files(true), "配布バージョンは書かない");
        let mut rec = FakeRecord::new(Vec::new(), 0);
        let rows = rec.copy_table();
        let mut s = Streams::open(&mut rec, rows, false);
        let batch: Vec<Snap> = (0..8u32)
            .map(|i| Snap::from_words(i, Box::new([0u32; SLOT_WORDS])))
            .collect();
        s.write_batch(&batch, &[]);
        for suffix in [TICKS_SUFFIX, CHANGES_SUFFIX, SNAP_SUFFIX, DRAWS_SUFFIX] {
            assert!(!rec.outputs.contains_key(suffix), "{suffix} を開いた");
        }
        assert_eq!((s.ticks.count, s.snap.count, s.snap_skipped), (0, 0, 0));
        let mut out = Vec::new();
        s.write_status(&mut out);
        let text = String::from_utf8(out).unwrap();
        for suffix in [TICKS_SUFFIX, CHANGES_SUFFIX, SNAP_SUFFIX, DRAWS_SUFFIX] {
            assert!(
                text.contains(&format!("{suffix} は書かない（配布バージョン）")),
                "{text}"
            );
        }
        assert!(!text.contains("開けない"), "{text}");
        let mut rec = FakeRecord::new(Vec::new(), 0);
        let rows = rec.copy_table();
        let _ = Streams::open(&mut rec, rows, true);
        for suffix in [TICKS_SUFFIX, CHANGES_SUFFIX, SNAP_SUFFIX, DRAWS_SUFFIX] {
            assert!(rec.outputs.contains_key(suffix), "{suffix} を開かない");
        }
    }

    #[test]
    fn the_dist_build_does_not_write_the_script_files() {
        assert!(writes_script_files(false), "開発用は書く");
        assert!(!writes_script_files(true), "配布バージョンは書かない");
    }

    #[test]
    fn a_script_run_opens_the_script_files_only_in_the_dev_build() {
        let mut h = clock_hook();
        let mut r = FakeRecord::new(sixty_fps(3), 3);
        let len = RunLen::Script {
            tail: 1,
            max: 3,
            steps: 0,
            feed_first: 0,
        };
        run_len(
            &mut h,
            &mut r,
            0,
            &mut Vec::new(),
            len,
            &trial_summary::installed_game_facts(),
        );
        let want = !cfg!(feature = "dist");
        assert_eq!(r.outputs.contains_key(FEED_SUFFIX), want, "{FEED_SUFFIX}");
        assert_eq!(r.outputs.contains_key(SHOTS_SUFFIX), want, "{SHOTS_SUFFIX}");
    }

    #[test]
    fn interim_sections_ignore_rtt_flips_and_roll_back_only_every_minute() {
        use trial_summary::{Item, Mark};
        let marks = |rtt: Mark, sync: Mark| {
            let item = |name, mark| Item {
                mark,
                name,
                text: String::new(),
            };
            trial_summary::marks_without_rtt(&[
                item(trial_summary::N_SYNC, sync),
                item(trial_summary::N_RTT, rtt),
                item(trial_summary::N_END, Mark::Skip),
            ])
        };
        let base = marks(Mark::Ok, Mark::Ok);
        let first = interim_due(None, &base, 1);
        assert_eq!((first.trial, first.rollback), (true, false));
        let mut last = base.clone();
        for (k, rtt) in [
            (2, Mark::Skip),
            (3, Mark::Ok),
            (4, Mark::Skip),
            (5, Mark::Ok),
        ] {
            let now = marks(rtt, Mark::Ok);
            assert_eq!(now, base, "RTT が比べに入っている");
            let due = interim_due(Some(&last), &now, k);
            assert_eq!((due.trial, due.rollback), (false, false), "{k} 秒");
            last = now;
        }
        let changed = marks(Mark::Ok, Mark::Bad);
        let due = interim_due(Some(&last), &changed, 6);
        assert_eq!((due.trial, due.rollback), (true, false));
        let due = interim_due(Some(&changed), &changed, SUMMARY_EVERY);
        assert_eq!((due.trial, due.rollback), (true, true));
    }

    #[test]
    fn net_runs_end_after_the_net_and_log_each_restart() {
        use crate::net_watch::restart_code;
        use higanr_core::handshake::{FailReason, Restart};
        fn snap(i: u32) -> NetSnapshot {
            // 0・1 回目: 繋がる前（作り直し 0 → 1）。2・3 回目: 作り直し 3（2 回ぶんまとめて増えた）。4 回目から: 終えた。
            let (phase, restarts) = match i {
                0 => (0, 0),
                1 => (0, 1),
                2 | 3 => (4, 3),
                _ => (5, 3),
            };
            NetSnapshot {
                phase,
                hs_restarts: restarts,
                hs_restart_last: restart_code(Some((FailReason::NothingReceived, Restart::Fresh))),
                ..NetSnapshot::default()
            }
        }
        let len = |max| RunLen::Net {
            tail: 3,
            max: Some(max),
            steps: 0,
            feed_first: 0,
        };
        let mut h = clock_hook();
        let mut rec = FakeRecord::new(Vec::new(), 0);
        rec.net = Some(snap);
        let mut out = Vec::new();
        run_len(
            &mut h,
            &mut rec,
            0,
            &mut out,
            len(100),
            &GameFacts::default(),
        );
        let log = String::from_utf8(out).unwrap();
        for n in 1..=3 {
            let line = format!("ネットワーク: 接続の手順を作り直した（{n} 回目。");
            assert_eq!(log.matches(&line).count(), 1, "{n} 回目: {log}");
        }
        assert!(!log.contains("（4 回目。"), "{log}");
        for n in [2, 3] {
            assert!(
                log.contains(&format!(
                    "{n} 回目。10 秒のあいだ相手から 1 通も届かなかった。最初から作り直した）（受信ソケットに届いた 0 通。捨てた受信: 候補に無い送り元 0 通 / タグの誤り 0 通 / 別のセッション 0 通）（3 秒の時点で見た）"
                )),
                "{n} 回目: {log}"
            );
        }
        assert!(
            log.contains("ネットワーク: 5 秒の時点で終わっていた"),
            "{log}"
        );
        assert!(log.contains("計測の集計: 8 回"), "{log}");
        assert!(
            log.contains("描画だけのフレームの予定: 作らない（ネットワークの実行）"),
            "{log}"
        );
        assert!(!log.contains("描画だけのフレーム: "), "{log}");
        let mut h = clock_hook();
        let mut rec = FakeRecord::new(Vec::new(), 0);
        let mut out = Vec::new();
        run_len(
            &mut h,
            &mut rec,
            0,
            &mut out,
            len(100),
            &GameFacts::default(),
        );
        let log = String::from_utf8(out).unwrap();
        assert!(
            log.contains(
                "ネットワーク: 1 秒の時点で終わっていた（ネットワークのセッションを置いていない）"
            ),
            "{log}"
        );
        assert!(log.contains("計測の集計: 4 回"), "{log}");
        let mut h = clock_hook();
        let mut rec = FakeRecord::new(Vec::new(), 0);
        rec.net = Some(|_| NetSnapshot {
            phase: 4,
            ..NetSnapshot::default()
        });
        let mut out = Vec::new();
        run_len(&mut h, &mut rec, 0, &mut out, len(6), &GameFacts::default());
        let log = String::from_utf8(out).unwrap();
        assert!(!log.contains("終わっていた"), "{log}");
        assert!(log.contains("計測の集計: 6 回"), "{log}");
    }

    #[test]
    fn a_net_run_without_a_cap_runs_past_the_script_cap_until_the_net_ends() {
        // ネットワークが終わる回（net_status の呼び出しの番号。0 始まりなので計測 END + 1 秒目に終わったと見える）。
        const END: u32 = SCRIPT_MAX_SECONDS + 300;
        fn snap(i: u32) -> NetSnapshot {
            NetSnapshot {
                phase: if i < END { 4 } else { 5 },
                ..NetSnapshot::default()
            }
        }
        let run_with = |max| {
            let mut h = clock_hook();
            let mut rec = FakeRecord::new(Vec::new(), 0);
            rec.net = Some(snap);
            let mut out = Vec::new();
            run_len(
                &mut h,
                &mut rec,
                0,
                &mut out,
                RunLen::Net {
                    tail: 3,
                    max,
                    steps: 0,
                    feed_first: 0,
                },
                &trial_summary::installed_game_facts(),
            );
            String::from_utf8(out).unwrap()
        };
        let log = run_with(None);
        let ended_at = END + 1;
        assert!(
            log.contains(&format!("ネットワーク: {ended_at} 秒の時点で終わっていた")),
            "{}",
            &log[log.len().saturating_sub(3000)..]
        );
        assert!(
            log.contains(&format!("計測の集計: {} 回", ended_at + 3)),
            "{}",
            &log[log.len().saturating_sub(3000)..]
        );
        assert!(log.contains("ネットワークが開いている間は計測の上限で止めない"));
        assert!(log.contains(&format!("計測 {}/上限なし: ", SCRIPT_MAX_SECONDS + 1)));
        assert!(log.contains(&format!(
            "==== 試験のまとめ（途中経過。計測 {} 秒の時点） ====",
            SCRIPT_MAX_SECONDS + SUMMARY_EVERY
        )));
        let log = run_with(Some(SCRIPT_MAX_SECONDS));
        assert!(!log.contains("終わっていた"));
        assert!(log.contains(&format!("計測の集計: {SCRIPT_MAX_SECONDS} 回")));
        assert!(log.contains(&format!(
            "（上限 {SCRIPT_MAX_SECONDS} 秒。ネットワークは計測の後も続く）"
        )));
        assert!(!log.contains("上限なし"));
    }

    #[test]
    fn trial_summary_is_written_when_the_marks_change() {
        use higanr_core::wire::Check;
        fn snap(i: u32) -> NetSnapshot {
            let chk = |rng| Check {
                seg: 0,
                frame: 60,
                rng_state: rng,
                rng_count: 9,
                life_1p: 10,
                life_2p: 10,
            };
            match i {
                0 | 1 => NetSnapshot::default(),
                2 | 3 => NetSnapshot {
                    phase: 4,
                    connected: true,
                    path_kind: 2,
                    segs_started: 1,
                    check_matches: 3,
                    agreed_mismatch: Some(0),
                    ..NetSnapshot::default()
                },
                _ => NetSnapshot {
                    phase: 5,
                    end: 4,
                    end_detail: 60,
                    connected: true,
                    path_kind: 2,
                    segs_started: 1,
                    check_matches: 3,
                    agreed_mismatch: Some(0),
                    desync: Some((chk(0x1111), chk(0x2222))),
                    ..NetSnapshot::default()
                },
            }
        }
        let len = RunLen::Net {
            tail: 3,
            max: Some(100),
            steps: 0,
            feed_first: 0,
        };
        let mut h = clock_hook();
        let mut rec = FakeRecord::new(Vec::new(), 0);
        rec.net = Some(snap);
        let mut out = Vec::new();
        run_len(
            &mut h,
            &mut rec,
            0,
            &mut out,
            len,
            &trial_summary::installed_game_facts(),
        );
        let log = String::from_utf8(out).unwrap();
        let heads: Vec<_> = log
            .lines()
            .filter(|l| l.starts_with("==== 試験のまとめ"))
            .collect();
        assert_eq!(
            heads,
            [
                "==== 試験のまとめ（途中経過。計測 1 秒の時点） ====",
                "==== 試験のまとめ（途中経過。計測 3 秒の時点） ====",
                "==== 試験のまとめ（途中経過。計測 5 秒の時点） ====",
            ],
            "{log}"
        );
        let last = &log[log.rfind("==== 試験のまとめ").unwrap()..];
        assert!(
            last.contains("× 同期: CHECK がずれた（seg 0 frame 60）"),
            "{last}"
        );
        assert!(
            last.contains("乱数の状態 0x1111") && last.contains("乱数の状態 0x2222"),
            "{last}"
        );
        assert!(last.contains("〇 繋がったか: 繋がった"), "{last}");
        assert_eq!(
            log.matches("ネットワーク: CHECK のズレ: 自分の CHECK: seg 0 frame 60 乱数の状態 0x1111 抽選回数 9 1P ライフ 10 2P ライフ 10 / 相手の CHECK: seg 0 frame 60 乱数の状態 0x2222")
                .count(),
            1,
            "{log}"
        );
        assert!(log.contains("（5 秒の時点で見た。"), "{log}");
        let mut h = clock_hook();
        let mut rec = FakeRecord::new(Vec::new(), 0);
        let mut out = Vec::new();
        run_len(
            &mut h,
            &mut rec,
            0,
            &mut out,
            RunLen::Net {
                tail: 3,
                max: Some(100),
                steps: 0,
                feed_first: 0,
            },
            &GameFacts::default(),
        );
        let log = String::from_utf8(out).unwrap();
        assert_eq!(log.matches("==== 試験のまとめ").count(), 1, "{log}");
    }

    /// 簡潔なログ用のネットワークの数: 繋がる → マッチが始まる → マッチが終わる → CHECK がずれる。
    fn brief_snap(i: u32) -> NetSnapshot {
        use higanr_core::wire::Check;
        let chk = |rng| Check {
            seg: 0,
            frame: 60,
            rng_state: rng,
            rng_count: 9,
            life_1p: 10,
            life_2p: 10,
        };
        let linked = NetSnapshot {
            connected: true,
            path_kind: 2,
            agreed_mismatch: Some(0),
            ..NetSnapshot::default()
        };
        match i {
            0 | 1 => NetSnapshot::default(),
            2 | 3 => NetSnapshot {
                phase: 4,
                segs_started: 1,
                check_matches: 3,
                ..linked
            },
            4 => NetSnapshot {
                phase: 1,
                segs_started: 1,
                check_matches: 3,
                ..linked
            },
            _ => NetSnapshot {
                phase: 5,
                end: 4,
                end_detail: 60,
                segs_started: 1,
                check_matches: 3,
                desync: Some((chk(0x1111), chk(0x2222))),
                ..linked
            },
        }
    }

    fn brief_run(snap: fn(u32) -> NetSnapshot) -> (String, FakeRecord) {
        let mut h = clock_hook();
        let mut rec = FakeRecord::new(Vec::new(), 0);
        rec.net = Some(snap);
        let mut out = Vec::new();
        crate::log_mode::with_detail(false, || {
            run_len(
                &mut h,
                &mut rec,
                0,
                &mut out,
                RunLen::Net {
                    tail: 3,
                    max: Some(100),
                    steps: 0,
                    feed_first: 0,
                },
                &trial_summary::installed_game_facts(),
            )
        });
        (String::from_utf8(out).unwrap(), rec)
    }

    /// 壊れたら落ちる: 簡潔なログが、毎秒の行・集計の節・全文の途中経過を書く。または、同期のズレ・マッチの節目・状態の変化を落とす。
    #[test]
    fn brief_log_keeps_events_and_anomalies_but_not_the_per_second_noise() {
        let (log, rec) = brief_run(brief_snap);
        for noise in [
            "計測 1/",
            "==== 集計",
            "TSC の周波数",
            "tick の集計",
            "書き出し:",
        ] {
            assert!(!log.contains(noise), "{noise} が簡潔なログに出た:\n{log}");
        }
        assert_eq!(
            log.matches("==== 試験のまとめ").count(),
            0,
            "全文の途中経過は書かない:\n{log}"
        );
        let heads: Vec<_> = log
            .lines()
            .filter(|l| l.starts_with("==== 状態の変化"))
            .collect();
        assert_eq!(
            heads,
            [
                "==== 状態の変化（計測 1 秒の時点） ====",
                "==== 状態の変化（計測 3 秒の時点） ====",
                "==== 状態の変化（計測 6 秒の時点） ====",
            ],
            "{log}"
        );
        // 最初は 〇 と × だけ、次は繋がった／マッチが始まった項目だけ、最後は同期のズレ。
        let first = log.split("==== 状態の変化（計測 3 秒").next().unwrap();
        assert!(first.contains("〇 花映塚のバージョン: "), "{first}");
        assert!(!first.contains("－ "), "最初の節に － が出た:\n{first}");
        let last = &log[log.rfind("==== 状態の変化").unwrap()..];
        assert!(
            last.contains("× 同期: CHECK がずれた（seg 0 frame 60）"),
            "{last}"
        );
        assert!(
            !last.contains("〇 花映塚のバージョン"),
            "変わらない項目を繰り返した:\n{last}"
        );
        assert!(log.contains("試合: 1 試合目が始まった"), "{log}");
        assert!(log.contains("試合: 1 試合目が終わった"), "{log}");
        assert_eq!(
            log.matches("ネットワーク: CHECK のズレ: 自分の CHECK: seg 0 frame 60")
                .count(),
            1,
            "{log}"
        );
        // 書いていない CSV を見比べろとは案内しない。詳細で再現する案内になる。
        assert!(!log.contains("_net_frames"), "{log}");
        assert!(log.contains("detail を 1 にして再現する"), "{log}");
        // 開いたファイルは無く、1 ms ごとのスレッドも起こさない。
        assert!(rec.outputs.is_empty(), "{:?}", rec.outputs.keys());
        assert!(!rec.menu_started);
    }

    /// 壊れたら落ちる: rollback のマッチをやめた理由（経路と確定したフレーム）が簡潔なログから消える。やめていない実行に出る。
    /// 同じ行を毎秒書く。
    #[test]
    fn brief_log_says_once_per_change_why_a_rollback_match_was_abandoned() {
        use crate::script_win::ROLLBACK_ABORT_HEAD;
        fn text(i: u32) -> String {
            match i {
                0..=2 => "受信 …\n巻き戻し（配線）: ふつうの行".to_string(),
                3..=4 => format!(
                    "受信 …\n{ROLLBACK_ABORT_HEAD} 1 回。最初は「x」（確定したフレーム 1259 の後）"
                ),
                _ => format!(
                    "受信 …\n{ROLLBACK_ABORT_HEAD} 2 回。最初は「x」（確定したフレーム 1259 の後）"
                ),
            }
        }
        let run = |net_text: fn(u32) -> String| {
            let mut h = clock_hook();
            let mut rec = FakeRecord::new(Vec::new(), 0);
            rec.net = Some(brief_snap);
            rec.net_text = Some(net_text);
            let mut out = Vec::new();
            crate::log_mode::with_detail(false, || {
                run_len(
                    &mut h,
                    &mut rec,
                    0,
                    &mut out,
                    RunLen::Net {
                        tail: 3,
                        max: Some(100),
                        steps: 0,
                        feed_first: 0,
                    },
                    &trial_summary::installed_game_facts(),
                )
            });
            String::from_utf8(out).unwrap()
        };
        let log = run(text);
        assert_eq!(log.matches("試合をやめた 1 回").count(), 1, "{log}");
        assert_eq!(log.matches("試合をやめた 2 回").count(), 1, "{log}");
        assert!(log.contains("確定したフレーム 1259 の後）（"), "{log}");
        let quiet = run(|_| "受信 …\n巻き戻し（配線）: ふつうの行".to_string());
        assert!(!quiet.contains("試合をやめた"), "{quiet}");
    }

    /// 壊れたら落ちる: 観戦の終わりの理由（ルームの失敗・終わりの記録）が簡潔なログから消える。受信の dump が簡潔に出る。
    #[test]
    fn brief_log_keeps_the_reason_a_watch_ended_but_not_the_receive_dump() {
        fn watch(i: u32) -> WatchLog {
            if i == 2 {
                WatchLog {
                    ended: vec![(
                        1,
                        "観戦者のルーム: 入った 1・ルームの失敗 2\n観戦者: 段階 終えた / 終わり 相手が黙った\n受信 Some(RecvStats { x })"
                            .to_string(),
                    )],
                    live: None,
                }
            } else {
                WatchLog::default()
            }
        }
        let mut h = clock_hook();
        let mut rec = FakeRecord::new(Vec::new(), 0);
        rec.net = Some(brief_snap);
        rec.watch = Some(watch);
        let mut out = Vec::new();
        crate::log_mode::with_detail(false, || {
            run_len(
                &mut h,
                &mut rec,
                0,
                &mut out,
                RunLen::Net {
                    tail: 3,
                    max: Some(100),
                    steps: 0,
                    feed_first: 0,
                },
                &trial_summary::installed_game_facts(),
            )
        });
        let log = String::from_utf8(out).unwrap();
        assert!(log.contains("==== 1 回目の観戦の終わり"), "{log}");
        assert!(log.contains("ルームの失敗 2"), "{log}");
        assert!(log.contains("終わり 相手が黙った"), "{log}");
        assert!(!log.contains("RecvStats"), "{log}");
    }

    /// 壊れたら落ちる: ネットワークの無い実行（`RunLen::Fixed`）が、簡潔なのに `_renderonly.csv` などを作る。詳細が作らない。
    #[test]
    fn a_fixed_run_writes_the_render_only_csv_only_when_detailed() {
        for (detail, want) in [(false, false), (true, true)] {
            let mut h = clock_hook();
            let mut rec = FakeRecord::new(Vec::new(), 0);
            let mut out = Vec::new();
            crate::log_mode::with_detail(detail, || run(&mut h, &mut rec, 0, &mut out, 3));
            assert_eq!(
                rec.outputs.contains_key(RENDERONLY_SUFFIX),
                want,
                "detail = {detail}: {:?}",
                rec.outputs.keys()
            );
            if !detail {
                assert!(rec.outputs.is_empty(), "{:?}", rec.outputs.keys());
            }
        }
    }

    /// 壊れたら落ちる: ネットワークの経路のパニックの行が、簡潔なログから消える。
    #[test]
    fn brief_log_reports_a_panic_in_the_net_path() {
        let mut h = clock_hook();
        let mut rec = FakeRecord::new(Vec::new(), 0);
        rec.net = Some(brief_snap);
        rec.host.net_panicked = true;
        let mut out = Vec::new();
        crate::log_mode::with_detail(false, || {
            run_len(
                &mut h,
                &mut rec,
                0,
                &mut out,
                RunLen::Net {
                    tail: 3,
                    max: Some(100),
                    steps: 0,
                    feed_first: 0,
                },
                &trial_summary::installed_game_facts(),
            )
        });
        let log = String::from_utf8(out).unwrap();
        assert!(
            log.contains("× パニック: 対戦の通信の入口 でパニックを受けて"),
            "{log}"
        );
    }

    /// 壊れたら落ちる: 簡潔と詳細で、書く行の数が逆転する・詳細が今までの行を失う。
    #[test]
    fn detail_log_still_has_the_per_second_lines_and_the_files() {
        let mut h = clock_hook();
        let mut rec = FakeRecord::new(Vec::new(), 0);
        rec.net = Some(brief_snap);
        let mut out = Vec::new();
        crate::log_mode::with_detail(true, || {
            run_len(
                &mut h,
                &mut rec,
                0,
                &mut out,
                RunLen::Net {
                    tail: 3,
                    max: Some(100),
                    steps: 0,
                    feed_first: 0,
                },
                &trial_summary::installed_game_facts(),
            )
        });
        let detail = String::from_utf8(out).unwrap();
        assert!(detail.contains("計測 1/"), "{detail}");
        assert!(detail.contains("==== 試験のまとめ（途中経過。計測 1 秒の時点） ===="));
        assert!(!detail.contains("==== 状態の変化"));
        assert!(rec.menu_started);
        let (brief, _) = brief_run(brief_snap);
        assert!(
            brief.lines().count() * 3 < detail.lines().count(),
            "簡潔 {} 行 / 詳細 {} 行",
            brief.lines().count(),
            detail.lines().count()
        );
    }

    #[test]
    fn rollback_summary_is_written_with_each_interim_summary() {
        fn snap(i: u32) -> NetSnapshot {
            if i < 2 {
                return NetSnapshot::default();
            }
            NetSnapshot {
                phase: 4,
                connected: true,
                path_kind: 2,
                segs_started: 1,
                check_matches: i,
                agreed_mismatch: Some(0),
                ..NetSnapshot::default()
            }
        }
        fn text(i: u32) -> String {
            let head = "受信 合成 / 開始のフックを通った 0 回";
            if i < 2 {
                return head.to_string();
            }
            format!(
                "{head}\n巻き戻し（製品の経路）: コピー {i}0\nネットワークの外の別の行 {i}\n巻き戻し（状態機械）: 書き戻し {i}1"
            )
        }
        let mut h = clock_hook();
        let mut rec = FakeRecord::new(Vec::new(), 0);
        rec.net = Some(snap);
        rec.net_text = Some(text);
        let mut out = Vec::new();
        run_len(
            &mut h,
            &mut rec,
            0,
            &mut out,
            RunLen::Net {
                tail: 3,
                max: Some(65),
                steps: 0,
                feed_first: 0,
            },
            &trial_summary::installed_game_facts(),
        );
        let log = String::from_utf8(out).unwrap();
        // 試験のまとめの途中経過は 1 秒目（繋がる前）・3 秒目（印が変わる）・60 秒目。巻き戻しの要約は 60 秒目だけ。
        let heads: Vec<_> = log
            .lines()
            .filter(|l| l.starts_with("==== 試験のまとめ（途中経過"))
            .collect();
        assert_eq!(heads.len(), 3, "{log}");
        assert_eq!(
            log.matches("---- 巻き戻しの要約（途中経過。").count(),
            1,
            "{log}"
        );
        let at = log
            .find("---- 巻き戻しの要約（途中経過。計測 60 秒の時点までの合計")
            .expect("60 秒目の巻き戻しの要約が無い");
        let tail: Vec<_> = log[at..].lines().skip(1).take(3).collect();
        assert_eq!(
            tail[..2],
            [
                "巻き戻し（製品の経路）: コピー 590",
                "巻き戻し（状態機械）: 書き戻し 591"
            ],
            "{log}"
        );
        assert!(tail[2].starts_with("計測 61/"), "{log}");
        assert!(
            !log[at..]
                .lines()
                .take(4)
                .any(|l| l.contains("ネットワークの外の別の行"))
        );
    }

    /// つなぎ直せる実行（`relink`）のネットワークの数: 呼んだ回数 `i`（1 秒に 1 回）で決める。1 本目は 0〜2 回目に繋がって進み、3〜7 回目は終わったまま、
    /// 8〜10 回目は次のセッションで 0 から置き直されて繋がり、11 回目から 2 本目も終わる。`ends` が偽なら 1 本も終わらない。
    fn relink_snap(i: u32, ends: bool) -> NetSnapshot {
        let ended = ends && ((3..8).contains(&i) || i >= 11);
        NetSnapshot {
            phase: if ended { 5 } else { 4 },
            end: if ended { 1 } else { 0 },
            connected: true,
            path_kind: 2,
            segs_started: 1,
            check_matches: i,
            agreed_mismatch: Some(0),
            relink: true,
            ..NetSnapshot::default()
        }
    }

    /// ネットワークの外の文: 巻き戻しの行の数をその回の番号にする（どの時点の文を書いたかを見る）。
    fn relink_text(i: u32) -> String {
        format!("受信 合成\n巻き戻し（製品の経路）: コピー {i}0\nネットワークの外の別の行 {i}")
    }

    fn run_relink(net: fn(u32) -> NetSnapshot) -> String {
        let mut h = clock_hook();
        let mut rec = FakeRecord::new(Vec::new(), 0);
        rec.net = Some(net);
        rec.net_text = Some(relink_text);
        let mut out = Vec::new();
        run_len(
            &mut h,
            &mut rec,
            0,
            &mut out,
            RunLen::Net {
                tail: 3,
                max: Some(20),
                steps: 0,
                feed_first: 0,
            },
            &trial_summary::installed_game_facts(),
        );
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_relinkable_run_writes_an_end_section_for_each_link_and_keeps_measuring() {
        let log = run_relink(|i| relink_snap(i, true));
        let heads: Vec<_> = log
            .lines()
            .filter(|l| l.contains("回目のつながりの終わり（計測"))
            .collect();
        assert_eq!(
            heads,
            [
                "==== 1 回目のつながりの終わり（計測 4 秒の時点。計測は続ける。次のつながりを待つ） ====",
                "==== 2 回目のつながりの終わり（計測 12 秒の時点。計測は続ける。次のつながりを待つ） ====",
            ],
            "{log}"
        );
        for (n, k, i) in [(1, 4, 3), (2, 12, 11)] {
            let at = log
                .find(&format!("==== {n} 回目のつながりの終わり（"))
                .unwrap();
            let sec = &log[at..];
            let sec = &sec[..sec.find(&format!("計測 {}/", k + 1)).expect(sec)];
            assert!(
                sec.contains(&format!(
                    "==== 集計（{n} 回目のつながりの終わり。始めからここまで） ===="
                )),
                "{sec}"
            );
            assert!(sec.contains("書き出し"), "{sec}");
            assert!(
                sec.contains(&format!(
                    "==== 試験のまとめ（{n} 回目のつながりの終わり。計測 {k} 秒の時点） ====\n"
                )),
                "{sec}"
            );
            assert!(sec.contains("〇 繋がったか: 繋がった"), "{sec}");
            assert!(
                sec.contains(&format!(
                    "---- 巻き戻しの要約（{n} 回目のつながりの終わり。計測 {k} 秒の時点までの合計。終わりの要約と同じ形） ----\n巻き戻し（製品の経路）: コピー {i}0\n"
                )),
                "{sec}"
            );
            assert!(!sec.contains("ネットワークの外の別の行"), "{sec}");
            assert!(!sec.contains("途中経過"), "{sec}");
        }
        assert!(!log.contains("==== 試験のまとめ ====\n"), "{log}");
        assert!(!log.contains("終わっていた"), "{log}");
        assert!(log.contains("計測の集計: 20 回"), "{log}");
    }

    #[test]
    fn link_end_sections_only_when_a_relinkable_link_ends() {
        let log = run_relink(|i| relink_snap(i, false));
        assert!(!log.contains("つながりの終わり"), "{log}");
        assert!(log.contains("計測の集計: 20 回"), "{log}");
        let log = run_relink(|i| NetSnapshot {
            relink: false,
            ..relink_snap(i, true)
        });
        assert!(!log.contains("つながりの終わり"), "{log}");
        assert!(
            log.contains("ネットワーク: 4 秒の時点で終わっていた"),
            "{log}"
        );
        assert!(log.contains("計測の集計: 7 回"), "{log}");
    }

    /// 1 本目は 0〜2 回目の読みで繋がって進み、3 回目から終わる。観戦者が 1 人入り（1 回目）、抜ける（終わり）。RTT の測定は 1 秒に 1 個。
    fn ending_link_snap(i: u32) -> NetSnapshot {
        let ended = i >= 3;
        NetSnapshot {
            phase: if ended { 5 } else { 4 },
            end: if ended { 5 } else { 0 },
            end_detail: u32::from(ended),
            connected: true,
            path_kind: 2,
            segs_started: 1,
            relink: true,
            link_started_ms: 4_000,
            played: 600,
            stalls: 10,
            stalls_sync: 6,
            stalls_ahead: 3,
            stalls_boundary: 1,
            rtt_ms: Some([170, 180, 176, 176][i.min(3) as usize]),
            rtt_pongs: i + 1,
            agreed_mismatch: Some(0),
            spectators: Some(crate::net_watch::SpectatorCounts {
                now: u32::from((1..3).contains(&i)),
                joined: u32::from(i >= 1),
                left: u32::from(ended),
                ..Default::default()
            }),
            ..NetSnapshot::default()
        }
    }

    /// 壊れたら落ちる: つながりの終わりの節に stall の内訳・RTT の中央値と最大（標本の数と PONG の数つき）・観戦者の数が出ない、
    /// 観戦者が来た・抜けた行が本ログに出ない / 毎秒出る、試合の終わりの行に接続が終わった理由が付かない。
    #[test]
    fn the_link_end_section_carries_the_stall_split_rtt_and_spectator_counts() {
        let mut h = clock_hook();
        let mut rec = FakeRecord::new(Vec::new(), 0);
        rec.net = Some(ending_link_snap);
        rec.net_text = Some(relink_text);
        let mut out = Vec::new();
        run_len(
            &mut h,
            &mut rec,
            0,
            &mut out,
            RunLen::Net {
                tail: 3,
                max: Some(12),
                steps: 0,
                feed_first: 0,
            },
            &trial_summary::installed_game_facts(),
        );
        let log = String::from_utf8(out).unwrap();
        assert_eq!(
            log.matches("観戦: 観戦者が来た（1 人。いま 1 人").count(),
            1,
            "{log}"
        );
        assert_eq!(
            log.matches("観戦: 観戦者が抜けた（自分で抜けた 1 人・黙って外した 0 人。いま 0 人")
                .count(),
            1,
            "{log}"
        );
        let at = log.find("==== 1 回目のつながりの終わり（").expect(&log);
        let sec = &log[at..];
        let sec = &sec[..sec.find("計測 5/").expect(sec)];
        for want in [
            "つながり: stall の内訳（フレーム数）: 時刻合わせ 6 / 先に行き過ぎ 3 / 境目 1 / 相手の入力待ち 0 / ほか 0（合計 10）。使ったフレーム 600 のうち 1.67%",
            "つながり: RTT 中央値 176 ms / 最大 180 ms（測れた PONG 4 個のうち、1 秒ごとの読み取りで採れた標本 4 個。見逃し 0 個）",
            "つながり: 観戦者 入った 1 人 / 自分で抜けた 1 人 / 黙って外した 0 人 / 上限で断った 0 人（終わりにいた 0 人）",
        ] {
            assert!(sec.contains(want), "{want}\n---\n{sec}");
        }
        assert_eq!(log.matches("つながり: stall の内訳").count(), 1, "{log}");
        // 試合の始まりと終わりの番号が揃い、終わりに接続の終わりの理由が付く。
        assert!(log.contains("試合: 1 試合目が始まった"), "{log}");
        assert!(
            log.contains("試合: 1 試合目が終わった（接続が終わったため（自分から終えた）"),
            "{log}"
        );
    }

    /// 壊れたら落ちる: 前の節と変わらない巻き戻しの要約・tick の集計・書き出しの行を、途中経過のたびに繰り返す。
    #[test]
    fn unchanged_interim_lines_are_not_repeated() {
        let mut h = clock_hook();
        let mut rec = FakeRecord::new(Vec::new(), 0);
        rec.net = Some(|_| NetSnapshot {
            phase: 4,
            connected: true,
            segs_started: 1,
            agreed_mismatch: Some(0),
            ..NetSnapshot::default()
        });
        rec.net_text = Some(|_| "受信 合成\n巻き戻し（製品の経路）: コピー 7".to_string());
        let mut out = Vec::new();
        run_len(
            &mut h,
            &mut rec,
            0,
            &mut out,
            RunLen::Net {
                tail: 3,
                max: Some(185),
                steps: 0,
                feed_first: 0,
            },
            &trial_summary::installed_game_facts(),
        );
        let log = String::from_utf8(out).unwrap();
        // 途中経過は 60・120・180 秒。同じ中身は最初の 1 回だけ。終わりの集計は毎回の全部を書く。
        assert_eq!(
            log.matches("---- 巻き戻しの要約（途中経過。").count(),
            1,
            "{log}"
        );
        assert_eq!(log.matches("==== 集計（途中 ").count(), 3, "{log}");
        assert_eq!(log.matches("tick の集計: 取れた 0 件").count(), 2, "{log}");
        assert_eq!(
            log.lines().filter(|l| l.starts_with("書き出し: ")).count(),
            2,
            "{log}"
        );
        assert_eq!(
            log.matches("集計: tick の集計と書き出しの行は、前の節から変わらないときは書かない")
                .count(),
            1,
            "{log}"
        );
    }

    /// 壊れたら落ちる: 持ちきれなかった件数が増えるだけの節を別の中身と見て毎回書く / 件数以外の変わりを同じと見て書かない。
    #[test]
    fn a_tick_summary_that_only_grew_its_overflow_counts_as_unchanged() {
        let base = Collected::default();
        let a = summarize(
            &Collected {
                overflow: 5,
                ..base.clone()
            },
            None,
        );
        let b = summarize(
            &Collected {
                overflow: 90_000,
                ..base.clone()
            },
            None,
        );
        assert!(same_but_overflow(&a, &b));
        let c = summarize(
            &Collected {
                lost: 1,
                ..base.clone()
            },
            None,
        );
        assert!(
            !same_but_overflow(&a, &c),
            "読み落としが増えたのに同じと見た"
        );
    }

    fn clock_hook() -> FakeHook {
        let mut f = FakeHook::new(0);
        f.now = 5_000;
        f.counter_fn = |now| ((now - 5_000) / 1000 * 60) as u32;
        f
    }

    #[test]
    fn a_scriptless_run_writes_the_outside_net_line_only_when_it_has_one() {
        for (has, want) in [(true, 1), (false, 0)] {
            let mut h = clock_hook();
            let mut r = FakeRecord::new(sixty_fps(180), 6);
            if has {
                r.net = Some(|_| NetSnapshot::default());
            }
            let mut out = Vec::new();
            run(&mut h, &mut r, 0, &mut out, 3);
            let log = String::from_utf8(out).unwrap();
            assert_eq!(
                log.lines()
                    .filter(|l| l.starts_with("ネットワークの外: "))
                    .count(),
                want,
                "{log}"
            );
        }
    }

    const JOIN_LINE: &str = "観戦者のルーム（合成）: 入った 1・返しを出した 1・つながった 1";

    fn run_watch(watch: Option<fn(u32) -> WatchLog>, seconds: u32) -> (String, FakeRecord) {
        let mut h = clock_hook();
        let mut r = FakeRecord::new(Vec::new(), 0);
        r.watch = watch;
        let mut out = Vec::new();
        run(&mut h, &mut r, 0, &mut out, seconds);
        (String::from_utf8(out).unwrap(), r)
    }

    #[test]
    fn a_closed_watch_is_written_once_at_the_next_round_with_its_room_counts() {
        let (log, r) = run_watch(
            Some(|n| WatchLog {
                ended: if n == 5 {
                    vec![(1, format!("{JOIN_LINE}\n受信 合成"))]
                } else {
                    Vec::new()
                },
                live: None,
            }),
            130,
        );
        let head = "==== 1 回目の観戦の終わり（計測 5 秒の時点。計測は続ける） ====";
        assert_eq!(log.matches("回目の観戦の終わり").count(), 1, "{log}");
        let at = log.find(head).expect(&log);
        assert!(log.find("計測 5/130:").unwrap() < at, "{log}");
        assert!(at < log.find("計測 6/130:").unwrap(), "{log}");
        assert!(log[at..].starts_with(&format!("{head}\n{JOIN_LINE}\n受信 合成\n")));
        assert!(!log.contains("観戦の途中経過"), "{log}");
        assert_eq!(r.watch_calls, 129, "最後の秒は取り出さない");
    }

    #[test]
    fn a_watching_run_gets_a_progress_section_every_summary_interval_and_a_run_without_one_gets_none()
     {
        let (log, _) = run_watch(
            Some(|_| WatchLog {
                ended: Vec::new(),
                live: Some(JOIN_LINE.to_string()),
            }),
            130,
        );
        let heads: Vec<_> = log
            .lines()
            .filter(|l| l.starts_with("---- 観戦の途中経過"))
            .collect();
        assert_eq!(
            heads,
            [
                "---- 観戦の途中経過（計測 60 秒の時点までの合計） ----",
                "---- 観戦の途中経過（計測 120 秒の時点までの合計） ----",
            ],
            "{log}"
        );
        assert_eq!(log.matches(JOIN_LINE).count(), 2, "{log}");
        assert!(!log.contains("観戦の終わり"), "{log}");
        let (log, r) = run_watch(None, 130);
        assert!(!log.contains("観戦の"), "{log}");
        assert!(!log.contains(JOIN_LINE), "{log}");
        assert_eq!(r.watch_calls, 129, "入れ物が無くても見回りは材料を問う");
    }

    #[test]
    fn a_closed_section_and_a_progress_section_can_share_a_second() {
        let (log, _) = run_watch(
            Some(|n| WatchLog {
                ended: if n == 60 {
                    vec![(2, "合成の節".to_string())]
                } else {
                    Vec::new()
                },
                live: Some(JOIN_LINE.to_string()),
            }),
            70,
        );
        let a = log
            .find("==== 2 回目の観戦の終わり（計測 60 秒")
            .expect(&log);
        let b = log.find("---- 観戦の途中経過（計測 60 秒").expect(&log);
        assert!(a < b, "{log}");
    }

    // 配るバージョンは観測の 4 つを開かないので、中身の `_ticks.csv` を読む検査は開発用だけ（配るバージョンで開かないことは
    // `the_dist_build_does_not_open_the_four_observe_files` が見る）。
    #[cfg(not(feature = "dist"))]
    #[test]
    fn a_hundred_thirty_seconds_of_sixty_fps() {
        let mut h = clock_hook();
        let mut r = FakeRecord::new(sixty_fps(7800), 6);
        let mut out = Vec::new();
        let (c, t) = run(&mut h, &mut r, 0, &mut out, 130);
        let log = String::from_utf8(out).unwrap();
        assert_eq!(
            c,
            ObserveSummary {
                samples: 130,
                total: 7800,
                min_step: 60,
                max_step: 60
            }
        );
        assert_eq!(h.sleeps, vec![POLL_MS; 1300]);
        assert_eq!(
            r.polls.get(),
            1301,
            "100 ms ごとの読み 1300 回と、最後の 1 回"
        );
        assert_eq!(r.pairs_read, 5);
        assert_eq!((t.records, t.lost), (7800, 0));
        assert!(t.exit_met());
        assert_eq!(log.lines().filter(|l| l.starts_with("計測 ")).count(), 130);
        assert!(
            log.contains("計測 1/130: 経過 1000 ms / 回数 60 / 増分 60"),
            "{log}"
        );
        assert!(log.contains("計測 130/130: 経過 130000 ms / 回数 7800 / 増分 60"));
        let heads: Vec<&str> = log.lines().filter(|l| l.starts_with("==== 集計")).collect();
        assert_eq!(
            heads,
            vec![
                "==== 集計（途中 60 秒。始めからここまで） ====",
                "==== 集計（途中 120 秒。始めからここまで） ====",
                "==== 集計（終わり。始めからここまで） ====",
            ]
        );
        assert!(log.contains("tick の集計: 取れた 3600 件"), "{log}");
        assert!(log.contains("tick の集計: 取れた 7200 件"), "{log}");
        assert!(log.contains("tick の集計: 取れた 7800 件"), "{log}");
        assert!(log.contains("TSC の周波数: 始めの 1 秒の QPC との対から 3000.000 MHz"));
        assert!(log.contains("ずれ +0.0 ppm"), "{log}");
        assert!(
            log.contains("コピーの表（入口が使っているもの。読み返し）: 6 行、合計 548 バイト")
        );
        assert!(log.contains(
            "書き出し: synthetic_ticks.csv に 7800 行 / synthetic_changes.csv に 92 行 / synthetic_snap.bin に 7800 件（1024 バイトずつ）"
        ), "{log}");
        assert!(r.menu_started && r.menu_stopped);
        assert!(
            log.contains("メニューとライフ（終わり）: 読み 0 回"),
            "{log}"
        );
        assert!(log.contains(
            "計測の集計: 130 回、合計の増分 7800、1 回（約 1000 ms）あたりの増分 最小 60 / 最大 60"
        ));
        let csv = r.output(TICKS_SUFFIX);
        let first_line = csv.lines().next().unwrap();
        assert_eq!(first_line, format!("{CSV_HEADER},{}", copies::csv_header()));
        assert_eq!(csv.lines().count(), 7801);
        assert!(csv.contains(&format!("7799,{:.3},", 7799.0 * FRAME_TSC as f64 * 1e6 / F)));
        let snap = r.outputs[SNAP_SUFFIX].0.borrow().len();
        assert_eq!(snap, copies::SNAP_HEADER_BYTES + 7800 * 1024);
        assert_eq!(r.output(CHANGES_SUFFIX).lines().count(), 1 + 92);
    }

    #[cfg(not(feature = "dist"))]
    #[test]
    fn rows_are_appended_every_second() {
        let mut h = clock_hook();
        let mut r = FakeRecord::new(sixty_fps(180), 6);
        let mut out = Vec::new();
        run(&mut h, &mut r, 0, &mut out, 3);
        let flushed = r.outputs[TICKS_SUFFIX].1.borrow().clone();
        assert_eq!(flushed, vec![61, 121, 181, 181]);
        let snap_flushed = r.outputs[SNAP_SUFFIX].1.borrow().len();
        assert_eq!(snap_flushed, 4, "_snap.bin も秒ごとに流す");
    }

    #[cfg(not(feature = "dist"))]
    #[test]
    fn zero_rows_leave_copy_columns_empty_and_say_so() {
        let mut h = clock_hook();
        let snaps: Vec<Snap> = (0..3).map(|i| snap_with(i, 100 + i, |_| 0x11)).collect();
        let mut r = FakeRecord::with_snaps(snaps, 3);
        r.rows = Vec::new();
        let mut out = Vec::new();
        run(&mut h, &mut r, 0, &mut out, 1);
        let log = String::from_utf8(out).unwrap();
        assert!(log.contains("★★ コピーの表: 入口は 0 行"), "{log}");
        let csv = r.output(TICKS_SUFFIX);
        let row = csv.lines().nth(2).unwrap();
        let n_copy = copies::csv_header().split(',').count();
        assert!(row.ends_with(&format!("0{}", ",".repeat(n_copy))), "{row}");
        assert_eq!(r.output(CHANGES_SUFFIX).lines().count(), 1, "見出しだけ");
        let mut h = clock_hook();
        let snaps: Vec<Snap> = (0..3).map(|i| snap_with(i, 100 + i, |_| 0x11)).collect();
        let mut r = FakeRecord::with_snaps(snaps, 3);
        run(&mut h, &mut r, 0, &mut Vec::new(), 1);
        let row = r.output(TICKS_SUFFIX).lines().nth(1).unwrap().to_string();
        assert!(row.contains(",0x1111,"), "{row}");
    }

    #[test]
    fn drain_counts_overwritten_and_retries_not_yet() {
        let mut r = FakeRecord::new(sixty_fps(20), 10);
        r.overwritten.insert(4);
        r.not_yet_once.borrow_mut().insert(7);
        let mut next = 0;
        let mut c = Collected::default();
        let mut batch = Vec::new();
        drain(&r, &mut next, &mut c, &mut batch);
        assert_eq!(next, 7);
        assert_eq!(c.lost, 1);
        assert_eq!(
            c.records.iter().map(|x| x.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 5, 6]
        );
        assert_eq!(
            batch.iter().map(|s| s.rec.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 5, 6]
        );
        drain(&r, &mut next, &mut c, &mut batch);
        assert_eq!(next, 20);
        assert_eq!(c.lost, 1);
        assert_eq!(c.records.len(), 19);
        assert_eq!(batch.len(), 19);
        assert!(
            c.records.windows(2).all(|w| w[0].seq < w[1].seq),
            "番号の順が崩れた"
        );
        assert_eq!(r.reads.borrow().iter().filter(|&&s| s == 7).count(), 2);
    }

    #[test]
    fn drain_skips_what_the_ring_no_longer_holds() {
        let n = RING_LEN + 904;
        let r = FakeRecord::new(sixty_fps(n), n);
        let mut next = 0;
        let mut c = Collected::default();
        let mut batch = Vec::new();
        drain(&r, &mut next, &mut c, &mut batch);
        assert_eq!(c.lost, 904);
        assert_eq!(c.records.len(), RING_LEN as usize);
        assert_eq!(c.records[0].seq, 904);
        assert_eq!(r.reads.borrow()[0], 904);
        assert_eq!(next, n);
    }

    #[test]
    fn drain_across_seq_wraparound() {
        let mut rs = sixty_fps(10);
        for (i, x) in rs.iter_mut().enumerate() {
            x.seq = (u32::MAX - 4).wrapping_add(i as u32);
        }
        let r = FakeRecord::new(rs, 10);
        let mut next = u32::MAX - 4;
        let mut c = Collected::default();
        drain(&r, &mut next, &mut c, &mut Vec::new());
        assert_eq!((c.records.len(), c.lost, next), (10, 0, 5));
    }

    #[cfg(not(feature = "dist"))]
    #[test]
    fn missing_pieces_are_reported_not_invented() {
        let mut h = clock_hook();
        let mut r = FakeRecord::new(sixty_fps(10), 1);
        r.pair_fn = |_| (5, 100);
        r.inv = None;
        r.divisor = None;
        r.open_fail.insert(TICKS_SUFFIX.to_string());
        r.menu_fail = true;
        let mut out = Vec::new();
        let (_, t) = run(&mut h, &mut r, 0, &mut out, 3);
        let log = String::from_utf8(out).unwrap();
        assert_eq!(t.interval, None);
        assert!(
            log.contains("TSC の周波数: 始めの 1 秒では求まらない"),
            "{log}"
        );
        assert!(
            log.contains("_ticks.csv は 開けない（合成の失敗）（それまでに 0 行）"),
            "{log}"
        );
        assert!(log.contains("synthetic_changes.csv に 92 行"), "{log}");
        assert!(log.contains("[0x004B30C4] は読めない"), "{log}");
        assert!(
            log.contains("分からない（CPUID に 0x80000007 の葉が無い）"),
            "{log}"
        );
        assert!(
            log.contains("メニューとライフ: 読み取りのスレッドを起こせない（合成の失敗）"),
            "{log}"
        );
        assert!(log.contains("メニューとライフ（終わり）: 読み取りのスレッドが無い"));
        assert!(log.contains("計測の集計: 3 回"), "{log}");
    }

    #[test]
    fn d3d8_probe_is_read_again_at_the_end() {
        let mut h = clock_hook();
        let mut r = FakeRecord::new(sixty_fps(6), 6);
        r.d3d8_by_call.insert(
            2,
            D3d8Probe {
                device: Some(0x1234_5678),
                vtable: Some(0x1000),
                endscene: Some(0x2000),
                present: Some(0x3000),
                endscene_module: Some("th09.exe".to_string()),
                present_module: Some("th09.exe".to_string()),
            },
        );
        let mut out = Vec::new();
        run(&mut h, &mut r, 0, &mut out, 1);
        let log = String::from_utf8(out).unwrap();
        assert_eq!(r.d3d8_calls.get(), 2, "始めと終わりの 2 回だけ読む");
        assert!(
            log.contains("D3D8 の下見: デバイス [0x4B3108] = 読めない"),
            "{log}"
        );
        assert!(
            log.contains("（終わりに読み直し）D3D8 の下見: デバイス [0x4B3108] = 0x12345678"),
            "{log}"
        );
        assert!(
            log.contains("（終わりに読み直し）D3D8 の下見: EndScene のモジュール = th09.exe"),
            "{log}"
        );
    }

    /// 壊れたら落ちる: 見張りが文字の描画の止まった理由を問わない・問っても本ログへ書かない・秒の時点を付けない。
    /// 落ちた後の調べで描画が止まっていたかが読めなくなる。
    #[test]
    fn overlay_stop_line_is_asked_every_second_and_written_to_the_brief_log() {
        let mut h = clock_hook();
        let mut r = FakeRecord::new(sixty_fps(6), 6);
        r.overlay_stop = Some((3, "文字の描画: 描画を止めた（合成）".to_string()));
        let mut out = Vec::new();
        run(&mut h, &mut r, 0, &mut out, 5);
        let log = String::from_utf8(out).unwrap();
        assert_eq!(r.overlay_stop_calls, 5, "1 秒ごとに問う");
        assert_eq!(
            log.matches("文字の描画: 描画を止めた（合成）（3 秒の時点で見た）")
                .count(),
            1,
            "{log}"
        );
    }

    #[test]
    fn counter_wraparound_gives_small_steps() {
        let mut h = FakeHook::new(0);
        h.now = 0;
        h.counter_fn = |now| (u32::MAX - 10).wrapping_add((now / 1000) as u32 * 60);
        let mut r = FakeRecord::new(Vec::new(), 0);
        let mut out = Vec::new();
        let (s, _) = run(&mut h, &mut r, 0, &mut out, 60);
        assert_eq!((s.total, s.min_step, s.max_step), (3600, 60, 60));
    }

    #[test]
    fn render_only_frames_only_in_the_dev_build_without_script_or_net() {
        let script = RunLen::Script {
            tail: 1,
            max: 2,
            steps: 0,
            feed_first: 0,
        };
        let net = RunLen::Net {
            tail: 1,
            max: Some(2),
            steps: 0,
            feed_first: 0,
        };
        assert!(makes_render_only(RunLen::Fixed(180), false));
        assert!(!makes_render_only(RunLen::Fixed(180), true));
        for dist in [false, true] {
            assert!(!makes_render_only(script, dist));
            assert!(!makes_render_only(net, dist));
        }
    }

    #[test]
    fn run_uses_the_render_only_decision_for_what_it_asks_and_what_it_logs() {
        let mut h = clock_hook();
        let mut r = FakeRecord::new(sixty_fps(6), 6);
        let mut out = Vec::new();
        run(&mut h, &mut r, 0, &mut out, 26);
        let log = String::from_utf8(out).unwrap();
        let dist = cfg!(feature = "dist");
        assert_eq!(makes_render_only(RunLen::Fixed(26), dist), !dist);
        if dist {
            assert!(r.render_only_asks.is_empty(), "{:?}", r.render_only_asks);
            assert!(
                log.contains("描画だけのフレームの予定: 作らない（配布バージョン）"),
                "{log}"
            );
            assert!(!log.contains("秒で N="), "{log}");
        } else {
            assert_eq!(r.render_only_asks, vec![1, 2, 3], "{log}");
            assert!(log.contains("描画だけのフレームの予定: 設置の後"), "{log}");
            assert!(!log.contains("作らない"), "{log}");
        }
    }
}
