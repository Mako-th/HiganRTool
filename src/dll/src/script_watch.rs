//! M1d の台本のある実行で、読み取りのスレッドが 100 ms ごとにすること（pure。unsafe 無し）: feed のリングバッファを読んで模型と照らし
//! `_feed.csv` へ、「できた」撮影の置き場を取って PNG と `_shots.csv` へ。終わりに集計をログへ。リングバッファ・置き場・進み具合は
//! `RecordEnv` 越しに読み、ファイルは `RecordEnv::open_output`（本ログの隣に回ごとに別名。既存を上書きしない）で開く。
//! 読み取りのスレッドで走る（確保もファイルも構わない）。
#![forbid(unsafe_code)]

use std::io::Write;

use crate::feeds::{FEED_CSV_HEADER, FeedChecker, FeedRecord, drain_feeds};
use crate::net_watch::{FrameChecker, NET_FRAMES_HEADER, NetFrame, csv_row};
use crate::observe::{RecordEnv, Sink};
use crate::shots::{H_BYTES, SHOTS_CSV_HEADER, ShotHead, ShotStats, Taken};

pub const FEED_SUFFIX: &str = "_feed.csv";
pub const SHOTS_SUFFIX: &str = "_shots.csv";
/// `.gz` で終わる名前は gzip で書く（`record_win::RealRecord::open_output`）。読む側は `higanr_core::gzlog::read_log_text` で
/// 平文と `.gz` の両方を読む。
pub const NET_FRAMES_SUFFIX: &str = "_net_frames.csv.gz";
/// 観戦者の実行だけ: `_net_frames.csv` と同じフレームを、`peer_sim --compare-spectate` が読む形（見出し `seg,frame,p1,p2` の
/// 4 列。`peer_sim::watch::frames_csv` と同じ）で書いたもの。`_net_frames.csv` の delay の列は観戦者に無い（ホストの delay と
/// 比べられない）ので、観戦者との突き合わせはこちらで行う。
pub const WATCH_FRAMES_SUFFIX: &str = "_watch_frames.csv.gz";
pub const WATCH_FRAMES_CSV_HEADER: &str = "seg,frame,p1,p2";

/// `WATCH_FRAMES_SUFFIX` の 1 行（入力ワードは 16 進 4 桁。`peer_sim::watch::frames_csv` と同じ形）。
pub fn watch_row(r: &crate::netplay::FrameRec) -> String {
    format!("{},{},{:04X},{:04X}", r.seg, r.frame, r.p1, r.p2)
}

pub struct ScriptWatch {
    steps: usize,
    next: u32,
    lost: u64,
    checker: FeedChecker,
    feed: Sink,
    shots: Sink,
    stats: ShotStats,
    batch: Vec<FeedRecord>,
    write_error: Option<String>,
    /// ネットワークを置いた実行だけ: 使ったフレームのリングバッファを読んで `_net_frames.csv` へ。
    net: Option<NetFrames>,
}

struct NetFrames {
    next: u32,
    lost: u64,
    checker: FrameChecker,
    sink: Sink,
    mirror: Option<Sink>,
    batch: Vec<NetFrame>,
}

impl NetFrames {
    /// `_net_frames.csv`（と、観戦者の実行なら `WATCH_FRAMES_SUFFIX` のコピー）を見出しつきで開く。
    fn open(rec: &mut dyn RecordEnv) -> NetFrames {
        let mut sink = Sink::open(rec, NET_FRAMES_SUFFIX);
        sink.write(format!("{NET_FRAMES_HEADER}\n").as_bytes());
        let mut n = NetFrames {
            next: 0,
            lost: 0,
            checker: FrameChecker::default(),
            sink,
            mirror: None,
            batch: Vec::new(),
        };
        n.ensure_mirror(rec, crate::spectate_watch::armed());
        n
    }

    /// 観戦者のコピーを、まだ開いていなければ開く（`armed` = 観戦者のセッションを置いた）。ゲーム内のメニューの実行は観戦者のセッションを
    /// **後から**置く（`script_win::install_placed`）ので、`_net_frames.csv` を開いた後に観戦者になりうる。置いた後の最初の
    /// 読みで開く（置く前の対戦者としてのフレームはコピーに入らない）。
    fn ensure_mirror(&mut self, rec: &mut dyn RecordEnv, armed: bool) {
        if armed && self.mirror.is_none() {
            let mut m = Sink::open(rec, WATCH_FRAMES_SUFFIX);
            m.write(format!("{WATCH_FRAMES_CSV_HEADER}\n").as_bytes());
            self.mirror = Some(m);
        }
    }
}

/// `_net_frames.csv.gz`（と観戦者のコピー）を書くか。簡潔なログでは開かず、リングバッファも読まない。
fn wants_net_frames(rec: &dyn RecordEnv) -> bool {
    crate::log_mode::detail() && rec.net_frames().is_some()
}

impl ScriptWatch {
    /// `_feed.csv` と `_shots.csv`（ネットワークを置いた実行では `_net_frames.csv` も）を開く（開けなくても計測は続ける。理由は集計に出る）。
    /// `feed_first` は台本を動かす前の feed のリングバッファの次の番号。ネットワークのリングバッファは始めから読む。`files` が false（配るバージョン。
    /// `observe::writes_script_files`）なら feed と shots は開かない（台本の実行でしか中身が入らず、見出しだけの空のファイルが残るため）。
    pub fn open(rec: &mut dyn RecordEnv, steps: usize, feed_first: u32, files: bool) -> Self {
        let sink = |rec: &mut dyn RecordEnv, suffix: &str| {
            if files {
                Sink::open(rec, suffix)
            } else {
                Sink::off(suffix)
            }
        };
        let mut feed = sink(rec, FEED_SUFFIX);
        feed.write(format!("{FEED_CSV_HEADER}\n").as_bytes());
        let mut shots = sink(rec, SHOTS_SUFFIX);
        shots.write(format!("{SHOTS_CSV_HEADER}\n").as_bytes());
        let net = wants_net_frames(rec).then(|| NetFrames::open(rec));
        ScriptWatch {
            steps,
            next: feed_first,
            lost: 0,
            checker: FeedChecker::default(),
            feed,
            shots,
            stats: ShotStats::default(),
            batch: Vec::new(),
            write_error: None,
            net,
        }
    }

    /// 1 回の読み（100 ms ごと）。`freq` は TSC の周波数（決まっていれば。`_shots.csv` の µs の列）。
    pub fn poll(&mut self, rec: &mut dyn RecordEnv, freq: Option<f64>) {
        // ゲーム内のメニューの実行（`code_side = game`）は、ネットワークを最初の「作る」「貼る」で開く。開いた後で `_net_frames.csv` を開く。
        if self.net.is_none() && wants_net_frames(rec) {
            self.net = Some(NetFrames::open(rec));
        }
        // 観戦者のセッションを後から置いた実行: コピーを遅れて開く（リングバッファを読む前に。置いたセッションの最初のフレームからコピーに入る）。
        if let Some(n) = self.net.as_mut() {
            n.ensure_mirror(rec, crate::spectate_watch::armed());
        }
        {
            let r: &dyn RecordEnv = rec;
            drain_feeds(
                r.feed_next_seq(),
                &|s| r.read_feed(s),
                &mut self.next,
                &mut self.lost,
                &mut self.batch,
            );
        }
        for r in core::mem::take(&mut self.batch) {
            let row = self.checker.check(&r);
            self.feed.line(&FeedChecker::row(&r, &row));
        }
        if let (Some(n), Some(ring)) = (self.net.as_mut(), rec.net_frames()) {
            ring.drain(&mut n.next, &mut n.lost, &mut n.batch);
            for f in core::mem::take(&mut n.batch) {
                n.checker.check(&f.rec);
                n.sink.line(&csv_row(&f));
                if let Some(m) = n.mirror.as_mut() {
                    m.line(&watch_row(&f.rec));
                }
            }
        }
        while let Some((slot, head)) = rec.take_shot() {
            let (n, what) = self.stats.take(&head);
            let file = match what {
                Taken::Write(suffix) => {
                    let pixels = rec.copy_shot(slot, head.get(H_BYTES) as usize);
                    rec.release_shot(slot);
                    match write_png(rec, &suffix, &head, &pixels) {
                        Ok(name) => {
                            self.stats.written += 1;
                            name
                        }
                        Err(e) => {
                            self.stats.write_failed += 1;
                            self.write_error.get_or_insert(e);
                            "write_failed".to_string()
                        }
                    }
                }
                Taken::OverLimit => {
                    rec.release_shot(slot);
                    "over_limit".to_string()
                }
                Taken::Failed => {
                    rec.release_shot(slot);
                    String::new()
                }
            };
            self.shots.line(&ShotStats::row(n, &head, freq, &file));
        }
    }

    pub fn flush(&mut self) {
        self.feed.flush();
        self.shots.flush();
        if let Some(n) = self.net.as_mut() {
            n.sink.flush();
            if let Some(m) = n.mirror.as_mut() {
                m.flush();
            }
        }
    }

    pub fn write_summary(&self, rec: &dyn RecordEnv, out: &mut dyn Write, freq: Option<f64>) {
        let _ = writeln!(out, "==== 台本（M1d） ====");
        let _ = writeln!(
            out,
            "台本: {}",
            rec.script_progress().text(Some(self.steps))
        );
        // 台本のマッチの状態の操作（無敵・ライフ・ゲージ）の記録は、ネットワークを置いた実行では「ネットワークの外」の文に入る（`script_win::net_status`）。
        // ここではネットワークの無い実行だけ。ゲーム内のメニューの実行はネットワークのセッションが無くても「ネットワークの外」の行を出し、その文に状態の操作の記録も
        // 入っているので、別に書かない。
        if self.net.is_none() && !write_outside_net(rec, out) {
            let _ = writeln!(out, "{}", crate::state_ops::STATE_LOG.text());
        }
        self.checker.write_summary(out, self.lost);
        self.stats.write_summary(out, &rec.shot_counters(), freq);
        if let Some(e) = &self.write_error {
            let _ = writeln!(out, "撮影: PNG を書けなかった最初の理由: {e}");
        }
        let _ = writeln!(
            out,
            "台本の書き出し: {} / {}",
            self.feed.status("行"),
            self.shots.status("行")
        );
        if let Some(n) = &self.net {
            let _ = writeln!(out, "==== ネットワーク（M2-2） ====");
            if let Some((snap, other)) = rec.net_status() {
                let _ = writeln!(out, "ネットワーク: {}", snap.text());
                let _ = writeln!(out, "ネットワークの外: {other}");
            }
            n.checker.write_summary(out, n.lost);
            let _ = writeln!(out, "ネットワークの書き出し: {}", n.sink.status("行"));
            if let Some(m) = &n.mirror {
                let _ = writeln!(
                    out,
                    "観戦者の書き出し（--compare-spectate 用）: {}",
                    m.status("行")
                );
            }
        }
    }
}

/// ネットワークのセッションを置いていない実行で、「ネットワークの外」の行を書く（書いたら true）。ゲーム内のメニューの実行はネットワークを開く前でもネットワークの外の文
/// （メニューの数え・ネットワークを開く仕事の文）を持つ（`RecordEnv::net_status`）。ネットワークを使わない実行は None で何も書かない。
pub fn write_outside_net(rec: &dyn RecordEnv, out: &mut dyn Write) -> bool {
    let Some((_, other)) = rec.net_status() else {
        return false;
    };
    let _ = writeln!(out, "ネットワークの外: {other}");
    true
}

fn write_png(
    rec: &mut dyn RecordEnv,
    suffix: &str,
    head: &ShotHead,
    pixels: &[u8],
) -> Result<String, String> {
    let png = head.to_png(pixels)?;
    let (name, mut w) = rec.open_output(suffix)?;
    w.write_all(&png)
        .and_then(|()| w.flush())
        .map_err(|e| format!("{name} に書けない: {e}"))?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    //! 読み取りのスレッドの側を、偽の `RecordEnv`（feed のリングバッファ・撮影の置き場・書き出し先）で通す。
    use super::*;
    use crate::feeds::tests::good_record;
    use crate::feeds::{FeedRead, im_from_bytes, im_to_bytes};
    use crate::menu::TickStats;
    use crate::observe::tests::SharedBuf;
    use crate::scripted::{Progress, RunState};
    use crate::shots::{
        C_GETBACKBUFFER, C_GETDESC, C_LOCKRECT, C_RELEASE, C_UNLOCKRECT, Fail, H_CALLS, H_FAIL,
        H_FORMAT, H_HEIGHT, H_K, H_REASON, H_SERIAL, H_WIDTH, HEAD_WORDS, ShotCounters,
    };
    use crate::ticks::{CopyRow, SlotRead};
    use higanr_core::input_model::{InputState, keys};
    use higanr_core::script::{POST, PRE};
    use higanr_core::shot::D3DFMT_X8R8G8B8;
    use std::cell::{Cell, RefCell};
    use std::collections::{HashMap, HashSet};

    /// 偽の記録側。feed のリングバッファは `feeds[seq]`（`visible` 件まで見える）、撮影の置き場は `shots`（置き場の番号・始まり・
    /// 画素。`release_shot` で消える）。
    #[derive(Default)]
    struct WatchRec {
        feeds: Vec<FeedRecord>,
        visible: u32,
        overwritten: HashSet<u32>,
        shots: Vec<(usize, ShotHead, Vec<u8>)>,
        /// `take_shot` の回数（置き場を空きに戻し忘れる変異で、検査が止まらずに落ちるための上限に使う）。
        takes: Cell<u32>,
        copied: RefCell<Vec<(usize, usize)>>,
        released: Vec<usize>,
        outputs: HashMap<String, SharedBuf>,
        open_fail: HashSet<String>,
        progress: Progress,
        /// ネットワークの外の文（None ならネットワークを使わない実行。`net_status` が None を返す）。
        outside: Option<String>,
        /// ネットワークの使ったフレームのリングバッファを持つ実行か。
        net_ring: bool,
    }

    static TEST_RING: crate::net_watch::NetFrameRing = crate::net_watch::NetFrameRing::new();

    impl RecordEnv for WatchRec {
        fn next_seq(&self) -> u32 {
            0
        }
        fn read_slot(&self, _seq: u32) -> SlotRead {
            SlotRead::NotYet
        }
        fn tsc_qpc(&mut self) -> (u64, i64) {
            (0, 0)
        }
        fn qpc_frequency(&self) -> i64 {
            10_000_000
        }
        fn invariant_tsc(&self) -> Option<bool> {
            None
        }
        fn game_clock_divisor(&mut self) -> Option<u32> {
            None
        }
        fn copy_table(&self) -> Vec<CopyRow> {
            Vec::new()
        }
        fn open_output(&mut self, suffix: &str) -> Result<(String, Box<dyn Write>), String> {
            if self.open_fail.contains(suffix) {
                return Err("合成の失敗".to_string());
            }
            let buf = self.outputs.entry(suffix.to_string()).or_default().clone();
            Ok((format!("synthetic{suffix}"), Box::new(buf)))
        }
        fn start_menu(&mut self) -> Result<String, String> {
            Err("使わない".to_string())
        }
        fn menu_stats(&mut self) -> Option<TickStats> {
            None
        }
        fn stop_menu(&mut self) -> Option<TickStats> {
            None
        }
        fn feed_next_seq(&self) -> u32 {
            self.visible
        }
        fn read_feed(&self, seq: u32) -> FeedRead {
            if self.overwritten.contains(&seq) {
                return FeedRead::Overwritten;
            }
            match self.feeds.get(seq as usize) {
                Some(r) if seq < self.visible => FeedRead::Record(Box::new(*r)),
                _ => FeedRead::NotYet,
            }
        }
        fn script_progress(&self) -> Progress {
            self.progress
        }
        fn take_shot(&mut self) -> Option<(usize, ShotHead)> {
            self.takes.set(self.takes.get() + 1);
            if self.takes.get() > 50 {
                return None;
            }
            self.shots.first().map(|(slot, head, _)| (*slot, *head))
        }
        fn copy_shot(&self, slot: usize, len: usize) -> Vec<u8> {
            self.copied.borrow_mut().push((slot, len));
            self.shots
                .iter()
                .find(|(s, _, _)| *s == slot)
                .map(|(_, _, px)| px[..len.min(px.len())].to_vec())
                .unwrap_or_default()
        }
        fn release_shot(&mut self, slot: usize) {
            self.released.push(slot);
            self.shots.retain(|(s, _, _)| *s != slot);
        }
        fn shot_counters(&self) -> ShotCounters {
            ShotCounters::default()
        }
        fn net_status(&self) -> Option<(crate::net_watch::NetSnapshot, String)> {
            self.outside
                .clone()
                .map(|o| (crate::net_watch::NetSnapshot::default(), o))
        }
        fn net_frames(&self) -> Option<&crate::net_watch::NetFrameRing> {
            self.net_ring.then_some(&TEST_RING)
        }
    }

    /// 壊れたら落ちる: 簡潔なログが `_net_frames.csv.gz` を開く（リングバッファを読み続ける）。詳細が開かない。
    #[test]
    fn the_net_frames_file_is_opened_only_when_detailed() {
        for (detail, want_file) in [(false, false), (true, true)] {
            let mut rec = WatchRec {
                net_ring: true,
                ..Default::default()
            };
            let w = crate::log_mode::with_detail(detail, || {
                let mut w = ScriptWatch::open(&mut rec, 0, 0, false);
                w.poll(&mut rec, None);
                w
            });
            assert_eq!(
                rec.outputs.contains_key(NET_FRAMES_SUFFIX),
                want_file,
                "detail = {detail}: {:?}",
                rec.outputs.keys()
            );
            assert_eq!(w.net.is_some(), want_file, "detail = {detail}");
        }
    }

    fn head(serial: u32, k: u32, reason: u8, fail: Fail) -> ShotHead {
        let mut h = [0u32; HEAD_WORDS];
        h[H_SERIAL] = serial;
        h[H_K] = k;
        h[H_REASON] = reason as u32;
        h[H_FAIL] = fail.code();
        h[H_CALLS] = C_GETBACKBUFFER | C_GETDESC | C_LOCKRECT | C_RELEASE;
        if fail == Fail::Ok {
            h[H_CALLS] |= C_UNLOCKRECT;
            h[H_WIDTH] = 2;
            h[H_HEIGHT] = 1;
            h[H_FORMAT] = D3DFMT_X8R8G8B8;
            h[H_BYTES] = 8;
        }
        ShotHead(h)
    }

    fn after_of(r: &FeedRecord) -> [InputState; 3] {
        core::array::from_fn(|i| im_from_bytes(&r.after[i]))
    }

    #[test]
    fn feeds_and_shots_reach_the_files_and_every_slot_is_released() {
        let s0 = [InputState::new(); 3];
        let r10 = good_record(10, keys::RIGHT, 0, s0);
        let r11 = good_record(11, keys::RIGHT, 0, after_of(&r10));
        let mut r12 = good_record(12, 0, keys::Z, after_of(&r11));
        let mut bad = im_from_bytes(&r12.after[1]);
        bad.repeat ^= keys::Z;
        r12.after[1] = im_to_bytes(&bad);
        let lost = good_record(13, 0, 0, after_of(&r12));
        let r14 = good_record(14, 0, 0, after_of(&lost));
        let mut rec = WatchRec {
            feeds: vec![r10, r11, r12, lost, r14],
            visible: 2,
            overwritten: HashSet::from([3]),
            shots: vec![
                (
                    2,
                    head(1, 50, PRE, Fail::Ok),
                    vec![0, 0, 0xFF, 0, 0xFF, 0, 0, 0],
                ),
                (0, head(2, 55, POST, Fail::LockRect), vec![0xAA; 8]),
                (1, head(3, 60, POST, Fail::Ok), vec![0x11; 8]),
            ],
            open_fail: HashSet::from(["_shot_0003_k60_post.png".to_string()]),
            progress: Progress {
                state: RunState::Done,
                step: 4,
                line: 0,
                ticks: 20,
                fed: 4,
                on: false,
            },
            ..Default::default()
        };
        let mut w = ScriptWatch::open(&mut rec, 5, 0, true);
        w.poll(&mut rec, None);
        rec.visible = 5;
        w.poll(&mut rec, None);
        w.flush();

        assert_eq!(
            rec.outputs[FEED_SUFFIX].text(),
            format!(
                "{FEED_CSV_HEADER}\n\
                 10,3,0x0080,0x0000,0x0010,ok,ok,ok,-\n\
                 11,3,0x0080,0x0000,0x0010,ok,ok,ok,ok\n\
                 12,3,0x0000,0x0001,0x0010,ok,repeat,ok,ok\n\
                 14,3,0x0000,0x0000,0x0010,ok,ok,ok,gap\n"
            )
        );

        assert_eq!(
            rec.released,
            vec![2, 0, 1],
            "どの置き場も 1 回ずつ空きに戻す"
        );
        assert_eq!(
            *rec.copied.borrow(),
            vec![(2, 8), (1, 8)],
            "コピーするのは撮れた置き場だけで、長さは先頭のバイト数"
        );
        let png_bytes = rec.outputs["_shot_0001_k50_pre.png"].0.borrow().clone();
        let mut reader = png::Decoder::new(std::io::Cursor::new(png_bytes))
            .read_info()
            .expect("PNG の先頭");
        let mut px = vec![0u8; reader.output_buffer_size().expect("大きさ")];
        let info = reader.next_frame(&mut px).expect("PNG の画素");
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(&px[..6], &[0xFF, 0, 0, 0, 0, 0xFF], "赤・青");
        assert!(
            !rec.outputs.contains_key("_shot_0002_k55_post.png"),
            "撮れなかった件を書いた"
        );

        let shots = rec.outputs[SHOTS_SUFFIX].text();
        let rows: Vec<&str> = shots.lines().collect();
        assert_eq!(rows.len(), 4, "{shots}");
        assert_eq!(rows[0], SHOTS_CSV_HEADER);
        assert!(
            rows[1].starts_with("1,1,50,0,pre,0,0,ok,2,1,22,"),
            "{}",
            rows[1]
        );
        assert!(
            rows[1].ends_with(",synthetic_shot_0001_k50_pre.png"),
            "{}",
            rows[1]
        );
        assert!(
            rows[2].starts_with("2,2,55,0,post,0,0,lockrect,0,0,0,"),
            "{}",
            rows[2]
        );
        assert!(
            rows[2].ends_with(','),
            "撮れなかった件に名前を書いた: {}",
            rows[2]
        );
        assert!(rows[3].ends_with(",write_failed"), "{}", rows[3]);

        let mut out = Vec::new();
        w.write_summary(&rec, &mut out, None);
        let log = String::from_utf8(out).unwrap();
        for want in [
            "台本: 最後の段まで終わった（全 5 段）。入口を通った tick 20 / feed した tick 4 / 分かれ道は閉じた",
            "feed の確かめ: リングバッファから読んだ件 4（台本で feed した tick）、3 本とも一致 3 件 / 食い違いのあった件 1 件、リングバッファの読み落とし 1 件",
            "feed の確かめ: im1（2P） の食い違い 合計 1（repeat 1）",
            "feed の確かめ: im0（1P） の食い違い 合計 0\n",
            "feed の続き: 続けて feed した組 2、前の後と次の前が違った組（間にゲームが書いた）0、k が飛んだ所 1",
            "撮影: 読み取りのスレッドが受け取った 3 / 撮れた 2 / 書いた 1 / 書けなかった 1 / 上限（400 枚）で書かなかった 0 / 撮れなかった（理由ごと）lockrect 1",
            "撮影: PNG を書けなかった最初の理由: 合成の失敗",
            "台本の書き出し: synthetic_feed.csv に 4 行 / synthetic_shots.csv に 3 行",
        ] {
            assert!(log.contains(want), "{want}\n----\n{log}");
        }
    }

    #[test]
    fn the_outside_net_line_is_written_without_a_net_box_only_when_the_run_has_it() {
        let stats = crate::state_ops::STATE_LOG.text();
        let mut rec = WatchRec {
            outside: Some(format!(
                "受信スレッドなし\n{stats}\nゲーム内のメニュー: 画面 6・7 で入力を握った 9 tick"
            )),
            ..WatchRec::default()
        };
        let w = ScriptWatch::open(&mut rec, 5, 0, true);
        assert!(w.net.is_none());
        let mut out = Vec::new();
        w.write_summary(&rec, &mut out, None);
        let log = String::from_utf8(out).unwrap();
        assert!(
            log.contains("ゲーム内のメニュー: 画面 6・7 で入力を握った 9 tick"),
            "{log}"
        );
        assert!(log.contains("ネットワークの外: 受信スレッドなし"), "{log}");
        assert!(!log.contains("==== ネットワーク（M2-2） ===="), "{log}");
        assert_eq!(
            log.matches(stats.as_str()).count(),
            1,
            "状態の操作の記録は「ネットワークの外」の文に入るので別には書かない: {stats}"
        );
        let mut rec = WatchRec::default();
        let w = ScriptWatch::open(&mut rec, 5, 0, true);
        let mut out = Vec::new();
        w.write_summary(&rec, &mut out, None);
        let log = String::from_utf8(out).unwrap();
        assert!(!log.contains("ネットワークの外"), "{log}");
        assert!(log.contains(stats.as_str()), "{log}");
        assert!(!write_outside_net(&rec, &mut Vec::new()));
    }

    #[test]
    fn script_files_are_opened_only_when_asked() {
        let mut rec = WatchRec::default();
        let _ = ScriptWatch::open(&mut rec, 5, 0, false);
        for suffix in [FEED_SUFFIX, SHOTS_SUFFIX] {
            assert!(!rec.outputs.contains_key(suffix), "{suffix} を開いた");
        }
        let mut rec = WatchRec::default();
        let _ = ScriptWatch::open(&mut rec, 5, 0, true);
        for suffix in [FEED_SUFFIX, SHOTS_SUFFIX] {
            assert!(rec.outputs.contains_key(suffix), "{suffix} を開かない");
        }
    }

    #[test]
    fn the_watch_mirror_opens_late_once_and_only_when_armed() {
        let mut rec = WatchRec::default();
        let mut n = NetFrames::open(&mut rec);
        assert!(n.mirror.is_none(), "観戦者の印が無いのにコピーを開いた");
        n.ensure_mirror(&mut rec, false);
        assert!(n.mirror.is_none());
        assert!(!rec.outputs.contains_key(WATCH_FRAMES_SUFFIX));
        n.ensure_mirror(&mut rec, true);
        assert!(n.mirror.is_some());
        let text = rec.outputs[WATCH_FRAMES_SUFFIX].text();
        assert_eq!(text, format!("{WATCH_FRAMES_CSV_HEADER}\n"), "{text:?}");
        n.ensure_mirror(&mut rec, true);
        assert_eq!(rec.outputs[WATCH_FRAMES_SUFFIX].text(), text);
    }

    #[test]
    fn the_watch_csv_is_read_by_compare_spectate_against_a_dll_csv() {
        use crate::netplay::FrameRec;
        use peer_sim::compare_spectate::{compare, parse_player, parse_watch_frames};
        let host: Vec<NetFrame> = (0..6u32)
            .map(|i| NetFrame {
                rec: FrameRec {
                    seg: (i / 3) as u16,
                    frame: i % 3,
                    p1: 0x0101 * (i as u16 + 1),
                    p2: 0x0002 + i as u16,
                },
                timing: crate::netplay::FrameTiming {
                    delay: 2,
                    margin_us: None,
                    wait_us: 0,
                    head_wait_us: 0,
                },
            })
            .collect();
        let player = format!(
            "{NET_FRAMES_HEADER}\n{}\n",
            host.iter().map(csv_row).collect::<Vec<_>>().join("\n")
        );
        // 観戦者は seg 0 の途中（frame 1）からでなく、seg の始まり（seg 1 の frame 0）から追った。
        let watch = format!(
            "{WATCH_FRAMES_CSV_HEADER}\n{}\n",
            host[3..]
                .iter()
                .map(|f| watch_row(&f.rec))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let w = parse_watch_frames(&watch).expect("観戦者の CSV が読めない");
        let p = parse_player(&player).expect("dll の CSV が読めない");
        assert_eq!((w.len(), p.len()), (3, 6));
        let o = compare(&w, &p);
        assert!(
            o.ok && o.offset == Some(3) && o.tail_left == Some(0),
            "{o:?}"
        );
        // 1P と 2P を入れ替えた観戦者の列は一致にならない（対照）。
        let swapped = watch.replace("0404,0005", "0005,0404");
        assert_ne!(swapped, watch, "対照の置き換えが当たっていない");
        let ws = parse_watch_frames(&swapped).unwrap();
        assert!(!compare(&ws, &p).ok);
    }

    #[test]
    fn loader_names_the_same_net_frames_file_as_the_dll_writes() {
        let loader = include_str!("../../loader/src/code_relay.rs");
        let line = format!("pub const NET_FRAMES_SUFFIX: &str = \"{NET_FRAMES_SUFFIX}\";");
        assert!(loader.contains(&line), "ローダーの定数が {line} でない");
        assert!(NET_FRAMES_SUFFIX.ends_with(higanr_core::gzlog::GZ_EXT));
    }

    #[test]
    fn net_frames_gz_round_trips_and_shrinks_for_a_realistic_shape() {
        use crate::net_watch::NetFrame;
        use crate::netplay::{FrameRec, FrameTiming};
        use higanr_core::gzlog::{self, GzLogWriter};
        let mut x: u32 = 0x1234_5678;
        let mut rnd = move || {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            x >> 8
        };
        let (mut p1, mut p2) = (0u16, 0u16);
        let mut plain = format!("{NET_FRAMES_HEADER}\n");
        let mut gz = GzLogWriter::new(Vec::new());
        gz.write_all(format!("{NET_FRAMES_HEADER}\n").as_bytes())
            .unwrap();
        let frames = 20 * 60 * 60;
        for f in 0..frames {
            if rnd() % 10 == 0 {
                p1 = (rnd() & 0x03FF) as u16;
            }
            if rnd() % 10 == 0 {
                p2 = (rnd() & 0x03FF) as u16;
            }
            let row = csv_row(&NetFrame {
                rec: FrameRec {
                    seg: (f / 10_000) as u16,
                    frame: f % 10_000,
                    p1,
                    p2,
                },
                timing: FrameTiming {
                    delay: 3,
                    margin_us: (rnd() % 8 != 0).then(|| 30_000 + rnd() % 6_000),
                    wait_us: rnd() % 40,
                    head_wait_us: if rnd() % 4 == 0 { rnd() % 1_500 } else { 0 },
                },
            });
            plain.push_str(&row);
            plain.push('\n');
            gz.write_all(row.as_bytes()).unwrap();
            gz.write_all(b"\n").unwrap();
        }
        let bytes = gz.finish().unwrap();
        let back = gzlog::text_from_bytes(bytes.clone()).unwrap();
        assert!(!back.cut);
        assert!(back.text == plain, "gzip を伸ばした行が平文と違う");
        let r = bytes.len() as f64 / plain.len() as f64;
        eprintln!(
            "合成 _net_frames.csv（20 分 {} 行）: 平文 {} B → gzip {} B（{:.1}%）",
            frames,
            plain.len(),
            bytes.len(),
            r * 100.0
        );
        assert!(r < 0.5, "{r}");
    }
}
