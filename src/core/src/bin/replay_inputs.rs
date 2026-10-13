//! リプレイ（`.rpy`）1 本から、枠（Match は枠 9）ごと・フレームごとの 1P / 2P の入力ワードを CSV で出し、
//! dll の `_net_frames.csv` と突き合わせる道具。
//!
//! 使い方:
//!   `replay_inputs <リプレイ.rpy> [--out <入力.csv>]`
//!   `replay_inputs <リプレイ.rpy> --compare <_net_frames.csv.gz か .csv> [--seg N] [--slot S] [--offset K] [--mask HEX]`
//!
//! - `--out` は CSV（`slot,frame,p1,p2,p3`。入力ワードは 16 進 4 桁）をファイルへ。既存の名前は上書きせず断る。
//!   `--compare` も `--out` も無ければ標準出力へ。
//! - `--compare`: csv の seg の N 行のうち、リプレイの同じフレームと 1P・2P とも一致した行を数え、最初の不一致を出す。
//!   `--seg` は csv に seg が 2 つ以上あるとき、`--slot` はリプレイに枠が 2 つ以上あるとき必須。
//!   `--offset K`: リプレイのフレーム = csv の frame + K（既定 `DEFAULT_OFFSET`）。合わなければ近い K の一致数を出す。
//!   `--mask HEX`: 両側にこのマスクを掛けてから比べる（既定 FFFF）。
//! - 一時停止の間のフレームはリプレイに入らず、押下（入力ワードの 0x0008）もリプレイでは 0 になる。csv は止まっている間も
//!   1 tick 1 行で進むので、押下の後の食い違いは、止まった行と再開の行を探してその間を飛ばし「食い違いでない行」に数える。
//!   csv の終わりの、リプレイの終わりより後の行（600 行まで）も食い違いに数えない。
//! - 全行一致でリプレイの終わりの方が csv に無ければ「csv が先に終わった」と出す。
//! - 終了コード: 0 = 全行一致（0 行は比べられていないので一致にしない）、1 = 不一致、2 = 使い方・読めない。
//!
//! リプレイは読むだけ。プレイヤー名・日付は出さない。
#![forbid(unsafe_code)]

use std::io::Write;
use std::process::ExitCode;

use higanr_core::gzlog;
use higanr_core::replay_compare::{self as cmp, NetRow};
use higanr_core::replay_decode::{self as dec, ReplayInputs, StageInputs};

const USAGE: &str = "使い方:\n  replay_inputs <リプレイ.rpy> [--out <入力.csv>]\n  replay_inputs <リプレイ.rpy> --compare <_net_frames.csv.gz か .csv> [--seg N] [--slot S] [--offset K] [--mask HEX]";

/// これより大きいファイルは読まない（壊れた指定でメモリを食わない）。
const MAX_FILE: u64 = 64 << 20;
/// 合わなかったときに一致数を出す offset の幅。記録の開始が 16 tick より前でも見つかるよう、開始の数秒分（300 tick）まで探す。
const SCAN_RANGE: i64 = 300;
/// リプレイの記録はネットワークの frame 0 の 1 tick 前から始まる（実測: 実機の複数マッチで offset 1 が全行一致、0 は大量に食い違い）。
const DEFAULT_OFFSET: i64 = 1;

#[derive(Debug, Default)]
struct Args {
    replay: String,
    out: Option<String>,
    compare: Option<String>,
    seg: Option<u16>,
    slot: Option<u8>,
    /// 指定が無ければ `DEFAULT_OFFSET`。指定の有無は「--compare の無い --offset」を断るのに使う。
    offset: Option<i64>,
    mask: u16,
}

impl Args {
    fn offset(&self) -> i64 {
        self.offset.unwrap_or(DEFAULT_OFFSET)
    }
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        mask: 0xFFFF,
        ..Args::default()
    };
    let mut it = args.iter();
    let mut replay: Option<String> = None;
    while let Some(x) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} に値が要る"))
        };
        match x.as_str() {
            "--out" => a.out = Some(value("--out")?),
            "--compare" => a.compare = Some(value("--compare")?),
            "--seg" => {
                a.seg = Some(value("--seg")?.parse().map_err(|_| "--seg は 0〜65535")?);
            }
            "--slot" => {
                a.slot = Some(value("--slot")?.parse().map_err(|_| "--slot は 0〜9")?);
            }
            "--offset" => {
                a.offset = Some(
                    value("--offset")?
                        .parse()
                        .map_err(|_| "--offset は整数（負でもよい）")?,
                );
            }
            "--mask" => {
                a.mask = u16::from_str_radix(value("--mask")?.trim_start_matches("0x"), 16)
                    .map_err(|_| "--mask は 16 進 4 桁まで")?;
            }
            s if s.starts_with("--") => return Err(format!("知らない引数 {s}")),
            s => {
                if replay.replace(s.to_string()).is_some() {
                    return Err("リプレイは 1 本だけ".to_string());
                }
            }
        }
    }
    a.replay = replay.ok_or("リプレイのパスが要る")?;
    if a.compare.is_none()
        && (a.seg.is_some() || a.slot.is_some() || a.offset.is_some() || a.mask != 0xFFFF)
    {
        return Err("--seg / --slot / --offset / --mask は --compare と一緒に使う".to_string());
    }
    Ok(a)
}

fn read_limited(path: &str) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{path} を読めない: {e}"))?;
    if meta.len() > MAX_FILE {
        return Err(format!("{path} が大きすぎる（{} B）", meta.len()));
    }
    std::fs::read(path).map_err(|e| format!("{path} を読めない: {e}"))
}

/// 名前・日付は出さない。
fn summary(r: &ReplayInputs) -> String {
    let mut s = format!(
        "game_mode {}（0 = Story / 1 = Extra / 2 = Match）、難易度 {}、枠 {} 個\n",
        r.game_mode,
        r.difficulty,
        r.stages.len()
    );
    for st in &r.stages {
        s.push_str(&format!(
            "  枠 {}: {} フレーム、1P キャラ {} human_cpu {}、2P キャラ {} human_cpu {}、C != (A|B) のフレーム {}、FPS 表 {} B（{}）\n",
            st.slot,
            st.frames(),
            st.p1.character,
            st.p1.human_cpu,
            st.p2.character,
            st.p2.human_cpu,
            st.p3_not_or_count(),
            st.fps.len(),
            if st.fps_len_ok { "長さは式どおり" } else { "★長さが式と違う" },
        ));
    }
    s
}

fn pick_stage(r: &ReplayInputs, want: Option<u8>) -> Result<&StageInputs, String> {
    let slots: Vec<u8> = r.stages.iter().map(|s| s.slot).collect();
    match want {
        Some(w) => r
            .stages
            .iter()
            .find(|s| s.slot == w)
            .ok_or_else(|| format!("リプレイに枠 {w} が無い（あるのは {slots:?}）")),
        None => match r.stages.as_slice() {
            [only] => Ok(only),
            [] => Err("リプレイに枠が 1 つも無い".to_string()),
            _ => Err(format!("リプレイに枠が {slots:?} とある。--slot で選ぶ")),
        },
    }
}

fn pick_seg(rows: &[NetRow], want: Option<u16>) -> Result<u16, String> {
    let segs = cmp::segs(rows);
    match want {
        Some(w) if segs.contains(&w) => Ok(w),
        Some(w) => Err(format!("csv に seg {w} が無い（あるのは {segs:?}）")),
        None => match segs.as_slice() {
            [only] => Ok(*only),
            [] => Err("csv に行が無い".to_string()),
            _ => Err(format!("csv に seg が {segs:?} とある。--seg で選ぶ")),
        },
    }
}

fn hex_pair(w: Option<(u16, u16)>) -> String {
    match w {
        Some((a, b)) => format!("1P {a:04X} 2P {b:04X}"),
        None => "（リプレイの範囲の外）".to_string(),
    }
}

fn report(rows: &[NetRow], st: &StageInputs, a: &Args, seg: u16, out: &mut String) -> bool {
    let offset = a.offset();
    let c = cmp::compare(rows, st, offset, a.mask);
    out.push_str(&format!(
        "比べた: リプレイの枠 {}（{} フレーム）と csv の seg {}（{} 行）、offset {}{}、mask {:04X}\n",
        st.slot,
        c.replay_frames,
        seg,
        c.csv_rows,
        offset,
        if a.offset.is_none() { "（既定）" } else { "" },
        a.mask
    ));
    out.push_str(&format!(
        "一致: csv の {} 行のうち {} 行（1P が違う {} 行 / 2P が違う {} 行 / リプレイの範囲の外 {} 行）\n",
        c.csv_rows, c.matched, c.p1_diff, c.p2_diff, c.out_of_range
    ));
    out.push_str(&format!(
        "食い違いでない行: 一時停止 {} 回（止まっていた間の行 {}）/ 一時停止の押下の行 {} / リプレイの終わりより後の行 {}。食い違い {} 行\n",
        c.pauses, c.paused_rows, c.pause_press, c.after_end, c.mismatched
    ));
    if c.csv_rows == 0 {
        out.push_str("★csv に行が無いので、比べられていない\n");
    }
    if let Some(d) = c.first_diff {
        out.push_str(&format!(
            "最初の不一致: csv の {} 行目（frame {}）: csv 1P {:04X} 2P {:04X} / リプレイ {}\n",
            d.row + 1,
            d.csv.frame,
            d.csv.p1,
            d.csv.p2,
            hex_pair(d.replay)
        ));
        let scan = cmp::scan_offsets(rows, st, a.mask, SCAN_RANGE);
        let top: Vec<String> = scan
            .iter()
            .take(3)
            .map(|(o, m)| format!("offset {o}: {m} 行"))
            .collect();
        out.push_str(&format!(
            "近い offset の一致（一時停止を考えない数え方。{SCAN_RANGE} まで。多い順）: {}\n",
            top.join(" / ")
        ));
    }
    // 全行一致のとき、残ったリプレイのフレームは csv が先に終わった分。リプレイはマッチの終わりで切れ、
    // ネットワークの列はその後の数秒も続くのが普通なので、残りがあれば csv の方が切れている。
    if c.csv_rows > 0 && c.first_diff.is_none() {
        let mapped = (c.csv_rows - c.paused_rows - c.after_end) as i64;
        let left = c.replay_frames as i64 - (offset.max(0) + mapped);
        if left > 0 {
            out.push_str(&format!(
                "csv が先に終わった（リプレイの最後の {left} フレームは csv に無い。計測の上限・ネットワークの終わり・ゲームを閉じた、のどれで切れたかは csv からは分からないので、本ログで見る）\n"
            ));
        }
    }
    c.all_match()
}

fn run(args: &[String]) -> Result<bool, String> {
    let a = parse_args(args)?;
    let file = read_limited(&a.replay)?;
    let r = dec::decode_inputs(&file).map_err(|e| format!("{} を復号できない: {e:?}", a.replay))?;
    eprint!("{}", summary(&r));

    let stdout = std::io::stdout();
    if let Some(path) = &a.out {
        let text = cmp::inputs_csv(&r);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .and_then(|mut f| f.write_all(text.as_bytes()))
            .map_err(|e| format!("{path} に書けない（同じ名前があれば上書きしない）: {e}"))?;
        eprintln!("{path} に書いた（{} B）", text.len());
    } else if a.compare.is_none() {
        stdout
            .lock()
            .write_all(cmp::inputs_csv(&r).as_bytes())
            .map_err(|e| format!("標準出力に書けない: {e}"))?;
    }

    let Some(csv_path) = &a.compare else {
        return Ok(true);
    };
    // 平文の CSV も `.csv.gz` も読む。強制終了で切れた gzip は読めた所まで。
    let t = gzlog::text_from_bytes(read_limited(csv_path)?)
        .map_err(|_| format!("{csv_path} が UTF-8 でない"))?;
    if let Some(note) = t.cut_note(csv_path) {
        eprintln!("{note}");
    }
    let text = t.text;
    let all = cmp::parse_net_frames(&text).map_err(|e| format!("{csv_path} を読めない: {e:?}"))?;
    let seg = pick_seg(&all, a.seg)?;
    let rows: Vec<NetRow> = all.into_iter().filter(|x| x.seg == seg).collect();
    let st = pick_stage(&r, a.slot)?;
    let mut out = String::new();
    let ok = report(&rows, st, &a, seg, &mut out);
    print!("{out}");
    Ok(ok)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
