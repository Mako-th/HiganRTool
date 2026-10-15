//! 記録: フレームごとの (seg, frame, 1P, 2P) の列と、終わりの要約（総フレーム数、stall の総数と最大連続、「到着 → 適用」の
//! p50 / p95 / p99 / max、捨てたものの数）。
//!
//! 回ごとに別名のファイルを `create_new` で作り、既存のファイルを上書きしない。置き場の中か上に `th09.exe` があれば（ゲームの
//! フォルダ）書かずに断る。

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use higanr_core::handshake::FailReason;

use crate::sim::{End, Report};

/// 同じ秒に同じ名前で何度作ってよいか（`_2`〜の尻を付けて探す回数）。
const NAME_ATTEMPTS: u32 = 100;

/// 最近順位法の百分位（`permille` は千分率。950 = p95）。並べた列で。空なら None。
pub fn percentile(sorted: &[u64], permille: u64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let n = sorted.len() as u64;
    // 順位 = ceil(n × p)。1 から数えるので、添字はその 1 つ前。
    let rank = (n * permille).div_ceil(1_000).max(1);
    sorted.get((rank - 1) as usize).copied()
}

pub(crate) fn ms3(us: Option<u64>) -> String {
    match us {
        Some(v) => format!("{}.{:03} ms", v / 1_000, v % 1_000),
        None => "-".to_string(),
    }
}

pub fn summary(rep: &Report) -> String {
    let mut s = String::new();
    let end = match rep.end {
        End::Finished { peer_bye: true } => {
            "全部の seg を終えた（相手の BYE を受けた）".to_string()
        }
        End::Finished { peer_bye: false } => {
            "全部の seg を終えた（相手の BYE は届かず、相手が 60 秒黙った）".to_string()
        }
        // 相手の HiganRTool のバージョンが違う: dll と同じ文字（自分は名乗ったバージョン）。
        End::HandshakeFailed(FailReason::OtherVersion { theirs }) => format!(
            "接続できなかった: {}",
            higanr_core::net_menu::other_version_text(rep.proto_ver, theirs)
        ),
        // 相手のビルドが違う（バージョン 5）: dll と同じ文字（自分は `update::BUILD_NUMBER`）。
        End::HandshakeFailed(FailReason::OtherBuild { theirs }) => format!(
            "接続できなかった: {}",
            higanr_core::net_menu::other_build_text(higanr_core::update::BUILD_NUMBER, theirs)
        ),
        End::HandshakeFailed(r) => format!("接続できなかった: {r:?}"),
        End::PeerBye(r) => format!("終える前に相手が BYE を出した: {r:?}"),
        End::Dead => "終える前に相手が 60 秒黙った（BYE(切断) を出した）".to_string(),
        End::Desync { local, remote } => {
            format!(
                "CHECK が合わなかった（BYE(ズレ検出) を出した）: 自分 {local:?} / 相手 {remote:?}"
            )
        }
        End::MenuDiverged { step, mismatch } => format!(
            "入った押下が選択の模型に合わなかった（BYE(取消) を出した）: {step:?} / {mismatch:?}"
        ),
        // 打ち切りで終えた回を、相手の側（dll の記録の「相手が終えた」）と突き合わせられるよう、BYE を出したかを添える。
        End::Deadline if rep.deadline_bye => format!(
            "打ち切りの時刻（{} ms）を過ぎた（--max-sec。相手へ BYE(終了) を 1 通出して終えた）",
            rep.max_ms
        ),
        End::Deadline => format!(
            "打ち切りの時刻（{} ms）を過ぎた（--max-sec。繋がる前か、もう閉じていた・切れていたので、ここでは BYE を出していない）",
            rep.max_ms
        ),
    };
    let _ = writeln!(s, "終わり方: {end}");
    if let Some(w) = rep.retry_window_ms {
        let _ = writeln!(
            s,
            "接続の手順: 時間切れの後も {w} ms まで作り直して待つ（--wait-sec）/ 作り直した {} 回 / 直前の 1 回 {:?}",
            rep.restarts.count, rep.restarts.last
        );
    }
    let _ = writeln!(
        s,
        "役割: {:?} / 席: {:?} / 始めの delay: {:?} / 相手の名前: {:?} / RTT: {:?} ms / 経過 {} ms",
        rep.role, rep.seat, rep.delay_start, rep.peer_name, rep.rtt_ms, rep.elapsed_ms
    );
    if rep.mode == Some(higanr_core::wire::Mode::Rollback) {
        let _ = writeln!(
            s,
            "モード: rollback（D {:?}。予測・進め直し・待ちの数 {:?}）",
            rep.delay_start, rep.rollback
        );
    }
    let _ = writeln!(
        s,
        "採った経路: {} / STUN: {}",
        rep.path.map_or("（無し）".to_string(), |p| p.to_string()),
        match rep.stun {
            None => "問い合わせない".to_string(),
            Some(higanr_core::stun::QueryState::Done(a)) => format!("見えた自分のアドレス {a}"),
            Some(q) => format!("{q:?}"),
        }
    );
    if rep.fps != higanr_core::pacing::DEFAULT_FPS || rep.one_way_ms != 0 {
        let _ = writeln!(
            s,
            "★検査用: ペース {} fps / 送りに足した一定の遅れ {} ms",
            rep.fps, rep.one_way_ms
        );
    }
    let frames = rep.recs.len();
    match rep.follow {
        // 付いていく形は相手のワードを待って使うのが常なので、stall は数えない（`sim.rs` のモジュール doc）。
        Some(f) => {
            let _ = writeln!(
                s,
                "総フレーム数 {frames} / 相手に付いていった（seg を進めた合図: 次の seg の INPUT {} 回・次の選択 {} 回・試合の外で使わなかった {} 回）",
                f.next_seg, f.peer_selecting, f.ignored
            );
        }
        None => {
            let _ = writeln!(
                s,
                "総フレーム数 {frames} / stall {} 回（{frames} フレームのうち。最大連続 {}）",
                rep.stalls, rep.max_stall_run
            );
        }
    }
    let mut a = rep.arrival_to_apply_us.clone();
    a.sort_unstable();
    let _ = writeln!(
        s,
        "到着 → 適用（{} フレームのうち {} 件。開始直後の穴埋めは届かないので数えない）: p50 {} / p95 {} / p99 {} / max {}",
        frames,
        a.len(),
        ms3(percentile(&a, 500)),
        ms3(percentile(&a, 950)),
        ms3(percentile(&a, 990)),
        ms3(a.last().copied())
    );
    let _ = writeln!(
        s,
        "CHECK の一致 {} 回 / 比べずに捨てた相手の CHECK {}",
        rep.check_matches, rep.check_dropped
    );
    for l in &rep.lat_results {
        let _ = writeln!(
            s,
            "latency の切替: seg {} frame {} → {}: {:?}",
            l.seg, l.frame, l.delay, l.result
        );
    }
    let _ = writeln!(s, "送り・受け・捨てたもの: {:?}", rep.counts);
    let _ = writeln!(s, "わざと壊した数: {:?}", rep.faults);
    let _ = writeln!(s, "接続の手順が捨てたもの: {:?}", rep.handshake);
    let _ = writeln!(s, "選択が捨てたもの: {:?}", rep.selection);
    let _ = writeln!(s, "lockstep の食い違いの数: {:?}", rep.lockstep);
    let _ = writeln!(s, "生存: {:?}", rep.liveness);
    if let Some(ag) = rep.agreed {
        let _ = writeln!(s, "揃った選択の値: {ag:?}");
    }
    let _ = writeln!(s, "入った画面を移る押下: {} 個", rep.presses.len());
    if let Some(sp) = &rep.spectate {
        let _ = writeln!(
            s,
            "観戦のポート: {} / 積んだ記録 {} 個 / 待って渡し終えた {} ms / 終わりに残った観戦者（送り元, 次に要る番号）{:?}",
            if sp.active {
                "ホストの役で流した"
            } else {
                "ホストの役でないので流していない"
            },
            sp.tail,
            sp.drain_ms,
            sp.spectators
        );
        let _ = writeln!(s, "観戦のポートの数え: {:?}", sp.counts);
        let _ = writeln!(
            s,
            "観戦者への送り {} 通（失敗 {}・ガードで止めた {}）/ わざと壊した数: {:?}",
            sp.sends, sp.send_errors, sp.send_refused, sp.faults
        );
    }
    if let Some(m) = &rep.menu {
        let _ = writeln!(
            s,
            "選択の台本: {} 項目のうち {} 項目を終えた / 入った選択 {} 回 / 難易度の画面の取消で外へ出た {} 回 / 要求を押し直した {} 回 / 相手の入力ワードが変わった {} 回",
            m.items,
            m.done,
            m.selections,
            m.left_to_mode,
            m.retries,
            m.remote_words.len()
        );
        match m.model {
            Some(md) => {
                let _ = writeln!(
                    s,
                    "選択の模型の終わり: 画面 {} / 難易度 {:?} / マップの一覧の値 {:?} / 1P {:?} / 2P {:?}",
                    md.screen(),
                    md.difficulty(),
                    md.map_item(),
                    md.decided(0),
                    md.decided(1)
                );
            }
            None => {
                let _ = writeln!(s, "選択の模型の終わり: 繋がらなかった");
            }
        }
    }
    s
}

pub fn log_text(rep: &Report, args_line: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# peer_sim の記録");
    let _ = writeln!(s, "# 引数: {args_line}");
    // 入った押下は `#` の行に置く（`#` でない行は (seg frame 1P 2P delay) の列だけにして、dll の
    // `_net_frames.csv` や相手の記録と行ごとに突き合わせられる形を保つ）。
    let _ = writeln!(s, "# 押下 step kind value screen（入った順）");
    for st in &rep.presses {
        let _ = writeln!(
            s,
            "# 押下 {} {:?} {:#04X} {}",
            st.step, st.kind, st.value, st.screen
        );
    }
    if let Some(m) = &rep.menu {
        let _ = writeln!(s, "# 相手のワード step word（選択の間に変わったところ）");
        for (step, w) in &m.remote_words {
            let _ = writeln!(s, "# 相手のワード {step} {w:04X}");
        }
    }
    let _ = writeln!(
        s,
        "# seg frame 1P 2P delay（delay はそのフレームを使った後の値）"
    );
    for r in &rep.recs {
        let _ = writeln!(
            s,
            "{} {} {:04X} {:04X} {}",
            r.seg, r.frame, r.p1, r.p2, r.delay
        );
    }
    let _ = writeln!(s, "# ---- 要約 ----");
    for line in summary(rep).lines() {
        let _ = writeln!(s, "# {line}");
    }
    s
}

pub fn refuse_game_folder(dir: &Path) -> Result<(), String> {
    let abs = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("今のフォルダが分からないので置き場を確かめられない: {e}"))?
            .join(dir)
    };
    for d in abs.ancestors() {
        if d.join("th09.exe").exists() {
            return Err(format!(
                "記録の置き場 {} はゲームのフォルダ {} の中なので書かない",
                abs.display(),
                d.display()
            ));
        }
    }
    Ok(())
}

fn safe(s: &str) -> String {
    let t: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    if t.is_empty() { "_".to_string() } else { t }
}

/// UTC の `YYYYMMDD_HHMMSSZ`（std だけで作る。地方時は std では取れない）。
pub fn utc_stamp(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // 1970-01-01 からの日数を年月日へ（civil_from_days の考え方）。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}_{:02}{:02}{:02}Z",
        rem / 3_600,
        rem / 60 % 60,
        rem % 60
    )
}

pub fn write_log(dir: &Path, name: &str, text: &str) -> Result<PathBuf, String> {
    write_named(dir, name, ".log", text)
}

/// `write_log` の、末尾（`suffix`。`.log`・`_watch_frames.csv` など）を選べる形。観戦者の CSV（`watch`）が使う。
pub fn write_named(dir: &Path, name: &str, suffix: &str, text: &str) -> Result<PathBuf, String> {
    refuse_game_folder(dir)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{} を作れない: {e}", dir.display()))?;
    let base = format!(
        "peer_sim_{}_{}_{}",
        utc_stamp(SystemTime::now()),
        safe(name),
        std::process::id()
    );
    for i in 0..NAME_ATTEMPTS {
        let file = if i == 0 {
            format!("{base}{suffix}")
        } else {
            format!("{base}_{}{suffix}", i + 1)
        };
        let path = dir.join(file);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(text.as_bytes())
                    .map_err(|e| format!("{} に書けない: {e}", path.display()))?;
                return Ok(path);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{} を開けない: {e}", path.display())),
        }
    }
    Err("記録の名前がすべて使われていた".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn percentile_nearest_rank() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 500), Some(50));
        assert_eq!(percentile(&v, 950), Some(95));
        assert_eq!(percentile(&v, 990), Some(99));
        assert_eq!(percentile(&v, 1_000), Some(100));
        assert_eq!(percentile(&[7], 990), Some(7));
        assert_eq!(percentile(&[1, 2, 3], 500), Some(2));
        assert_eq!(percentile(&[], 500), None);
    }

    #[test]
    fn utc_stamp_known_dates() {
        assert_eq!(utc_stamp(UNIX_EPOCH), "19700101_000000Z");
        assert_eq!(
            utc_stamp(UNIX_EPOCH + Duration::from_secs(951_827_696)),
            "20000229_123456Z"
        );
        assert_eq!(
            utc_stamp(UNIX_EPOCH + Duration::from_secs(1_790_553_600)),
            "20260928_000000Z"
        );
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "higanrtool_peer_sim_{tag}_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn second_write_gets_another_name_and_first_is_kept() {
        let d = temp_dir("names");
        let a = write_log(&d, "x y", "first\n").unwrap();
        let b = write_log(&d, "x y", "second\n").unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "first\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "second\n");
        assert!(
            a.file_name().unwrap().to_string_lossy().contains("_x_y_"),
            "{a:?}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn game_folder_and_below_are_refused() {
        let d = temp_dir("game");
        assert!(write_log(&d.join("logs"), "a", "ok\n").is_ok());
        std::fs::write(d.join("th09.exe"), b"").unwrap();
        assert!(refuse_game_folder(&d).is_err());
        let e = write_log(&d.join("logs").join("deeper"), "a", "no\n").unwrap_err();
        assert!(e.contains("ゲームのフォルダ"), "{e}");
        assert!(
            !d.join("logs").join("deeper").exists(),
            "断ったのにフォルダを作った"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
