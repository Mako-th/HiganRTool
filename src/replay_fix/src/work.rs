//! リプレイを読み、直した写しをファイルに書く。元のファイルには書かない。

use std::io::Write;
use std::path::{Path, PathBuf};

use higanr_core::replay_rewrite::{FixError, PlanError, fixed_copy, plan};

use crate::model::{Analysis, Kind, Pair, output_path};

/// これより大きいファイルは読まない（壊れた指定でメモリを食わない）。
const MAX_FILE: u64 = 64 << 20;

fn read(path: &Path) -> Result<Vec<u8>, String> {
    let len = std::fs::metadata(path)
        .map_err(|e| format!("開けない: {e}"))?
        .len();
    if len > MAX_FILE {
        return Err(format!("大きすぎる（{len} バイト）"));
    }
    std::fs::read(path).map_err(|e| format!("読めない: {e}"))
}

pub fn analyze(path: &Path) -> Analysis {
    match read(path) {
        Ok(bytes) => Analysis::from_plan(plan(&bytes)),
        Err(e) => Analysis::Unreadable(e),
    }
}

/// 1 本の結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MakeResult {
    /// 書いた先。
    Made(PathBuf),
    /// 直す値が元の値と同じ。
    Unchanged,
    /// 印に `charge` がある。
    Recorded,
    NoMark,
    /// 見出しの値が 0 / 1 のどちらでもない。
    OddValue(u8, u8),
    /// 書く先に同じ名前のファイルが既にある（上書きしない）。
    Exists(PathBuf),
    Failed(String),
}

enum WriteError {
    Exists,
    Other(String),
}

/// 既にあれば書かない。`create_new` なので、確かめてから書く間に作られても上書きしない。
fn write_new(dst: &Path, bytes: &[u8]) -> Result<(), WriteError> {
    if let Some(d) = dst.parent() {
        std::fs::create_dir_all(d)
            .map_err(|e| WriteError::Other(format!("{} を作れない: {e}", d.display())))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => WriteError::Exists,
            _ => WriteError::Other(format!("{} を開けない: {e}", dst.display())),
        })?;
    f.write_all(bytes)
        .map_err(|e| WriteError::Other(format!("{} に書けない: {e}", dst.display())))
}

/// `src` の写しを、見出しと印のチャージ型を `pair` にして `output_path` に書く。
pub fn make(src: &Path, pair: Pair) -> MakeResult {
    let bytes = match read(src) {
        Ok(b) => b,
        Err(e) => return MakeResult::Failed(e),
    };
    match plan(&bytes) {
        Ok(p) if Kind::from_u8(p.current.0).is_none() || Kind::from_u8(p.current.1).is_none() => {
            return MakeResult::OddValue(p.current.0, p.current.1);
        }
        Ok(_) => {}
        Err(PlanError::NoMark) => return MakeResult::NoMark,
        Err(PlanError::Recorded(_) | PlanError::Newer(_)) => return MakeResult::Recorded,
        Err(PlanError::Decode(e)) => return MakeResult::Failed(format!("{e:?}")),
    }
    let fixed = match fixed_copy(&bytes, pair.0.value(), pair.1.value()) {
        Ok(f) => f,
        Err(FixError::Unchanged) => return MakeResult::Unchanged,
        Err(FixError::Plan(PlanError::Recorded(_) | PlanError::Newer(_))) => {
            return MakeResult::Recorded;
        }
        Err(e) => return MakeResult::Failed(format!("{e:?}")),
    };
    let Some(dst) = output_path(src, pair) else {
        return MakeResult::Failed("ファイル名が無い".to_string());
    };
    match write_new(&dst, &fixed) {
        Ok(()) => MakeResult::Made(dst),
        Err(WriteError::Exists) => MakeResult::Exists(dst),
        Err(WriteError::Other(e)) => MakeResult::Failed(e),
    }
}

/// 一括の結果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub made: Vec<PathBuf>,
    pub unchanged: usize,
    /// 直し済みか v1.0.0 より新しいため直さなかったもの。(対象の並びでの位置, パス)。
    pub recorded: Vec<(usize, PathBuf)>,
    pub exists: usize,
    pub no_mark: usize,
    pub odd_value: usize,
    pub failed: Vec<(PathBuf, String)>,
}

impl Report {
    /// 対象の `index` 番目 `src` の結果 `result` を数える。
    pub fn add(&mut self, index: usize, src: &Path, result: MakeResult) {
        match result {
            MakeResult::Made(dst) => self.made.push(dst),
            MakeResult::Unchanged => self.unchanged += 1,
            MakeResult::Recorded => self.recorded.push((index, src.to_path_buf())),
            MakeResult::Exists(_) => self.exists += 1,
            MakeResult::NoMark => self.no_mark += 1,
            MakeResult::OddValue(..) => self.odd_value += 1,
            MakeResult::Failed(e) => self.failed.push((src.to_path_buf(), e)),
        }
    }

    /// 対象の本数。
    pub fn total(&self) -> usize {
        self.made.len() + self.skipped()
    }

    /// 作った写しのフォルダ（`<型>-<型>`）。重ならない順。
    pub fn output_dirs(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = Vec::new();
        for d in self.made.iter().filter_map(|p| p.parent()) {
            if !v.iter().any(|x| x == d) {
                v.push(d.to_path_buf());
            }
        }
        v
    }

    /// 作らなかった本数。
    pub fn skipped(&self) -> usize {
        self.unchanged
            + self.recorded.len()
            + self.exists
            + self.no_mark
            + self.odd_value
            + self.failed.len()
    }

    /// 直したいのに作れなかったものがあるか（「変えるものが無い」「直し済み」は含まない）。
    pub fn has_problem(&self) -> bool {
        self.exists + self.no_mark + self.odd_value + self.failed.len() > 0
    }
}

/// `paths` のそれぞれに同じ `pair` で `make` する。
pub fn make_all(paths: &[PathBuf], pair: Pair) -> Report {
    let mut r = Report::default();
    for (i, p) in paths.iter().enumerate() {
        r.add(i, p, make(p, pair));
    }
    r
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use higanr_core::replay::{
        Mark, MarkRead, MarkRole, MatchResult, Side, Timestamp, chunk_to_append, read_mark,
    };
    use higanr_core::replay_build::{BuildSpec, build_replay};
    use higanr_core::replay_decode::decode_image;
    use higanr_core::replay_detail::MatchDetail;
    use higanr_core::replay_rewrite::{charge, with_charge};

    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rfix_work_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 合成のリプレイ（実在の名前・実物のリプレイは使わない）。見出しは (`h1`, `h2`)、印の名前は Alice（自分）と Bob（相手）。
    pub(crate) fn replay(
        h1: u8,
        h2: u8,
        side: Side,
        detail: Option<MatchDetail>,
        marked: bool,
    ) -> Vec<u8> {
        let base = build_replay(&BuildSpec::match_replay(
            0x60,
            (0..300u16).map(|i| i % 7).collect(),
            (0..300u16).map(|i| (i / 3) % 5).collect(),
        ));
        let mut f = with_charge(&base, h1, h2).unwrap();
        if marked {
            let mark = Mark {
                peer: "Bob".to_string(),
                self_name: "Alice".to_string(),
                side,
                lat: 4,
                ping_ms: 50,
                result: MatchResult::Win,
                at: Timestamp {
                    year: 2026,
                    month: 10,
                    day: 14,
                    hour: 1,
                    minute: 2,
                    second: 3,
                },
                role: MarkRole::Play,
                detail,
            };
            let chunk = chunk_to_append(&f, &mark).unwrap();
            f.extend_from_slice(&chunk);
        }
        f
    }

    fn header_of(path: &Path) -> (u8, u8) {
        charge(&decode_image(&std::fs::read(path).unwrap()).unwrap()).unwrap()
    }

    fn mark_charge(path: &Path) -> Option<[u8; 2]> {
        let MarkRead::Found(m) = read_mark(&std::fs::read(path).unwrap()).unwrap() else {
            panic!("印が読めない");
        };
        m.detail.and_then(|d| d.charge)
    }

    #[test]
    fn making_writes_a_marked_copy_into_the_pair_folder_and_leaves_the_source_alone() {
        let d = temp_dir("make");
        let rep = d.join("Alice vs Bob");
        std::fs::create_dir_all(&rep).unwrap();
        let src = rep.join("th9_ud0008.rpy");
        let bytes = replay(0, 0, Side::P2, None, true);
        std::fs::write(&src, &bytes).unwrap();
        let got = make(&src, (Kind::Charge, Kind::Slow));
        let dst = d
            .join("fix_replay")
            .join("Alice vs Bob")
            .join("charge-slow")
            .join("th9_ud0008.rpy");
        assert_eq!(got, MakeResult::Made(dst.clone()));
        assert_eq!(header_of(&dst), (1, 0), "見出しの 1P / 2P が逆");
        assert_eq!(
            mark_charge(&dst),
            Some([1, 0]),
            "印に charge が入っていない"
        );
        assert_eq!(
            std::fs::read(&src).unwrap(),
            bytes,
            "元のファイルが変わった"
        );
        // 同じものをもう一度作ると上書きせずに知らせる。
        let written = std::fs::read(&dst).unwrap();
        assert_eq!(
            make(&src, (Kind::Charge, Kind::Slow)),
            MakeResult::Exists(dst.clone())
        );
        assert_eq!(std::fs::read(&dst).unwrap(), written);
        // 直した写しは「直し済み」なので、もう一度直せない。
        assert_eq!(
            analyze(&dst),
            Analysis::Recorded,
            "直した写しが直し済みの印を持たない"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn nothing_is_written_when_the_values_are_the_same_or_the_replay_cannot_be_fixed() {
        let d = temp_dir("refuse");
        let rep = d.join("rep");
        std::fs::create_dir_all(&rep).unwrap();
        let write = |name: &str, b: Vec<u8>| {
            let p = rep.join(name);
            std::fs::write(&p, b).unwrap();
            p
        };
        let same = write("same.rpy", replay(0, 1, Side::P1, None, true));
        let done = write(
            "done.rpy",
            replay(
                0,
                1,
                Side::P1,
                Some(MatchDetail {
                    charge: Some([0, 1]),
                    ..Default::default()
                }),
                true,
            ),
        );
        let plain = write("plain.rpy", replay(0, 1, Side::P1, None, false));
        let odd = write("odd.rpy", replay(7, 1, Side::P1, None, true));
        let not_replay = write("not_replay.rpy", b"not a replay".to_vec());
        let mut damaged_bytes = replay(0, 1, Side::P1, None, true);
        damaged_bytes[0x40] ^= 0xFF;
        let damaged = write("damaged.rpy", damaged_bytes);
        let pair = (Kind::Slow, Kind::Slow);
        assert_eq!(
            make(&same, (Kind::Slow, Kind::Charge)),
            MakeResult::Unchanged
        );
        assert_eq!(make(&done, pair), MakeResult::Recorded);
        assert_eq!(make(&plain, pair), MakeResult::NoMark);
        assert_eq!(make(&odd, pair), MakeResult::OddValue(7, 1));
        assert_eq!(make(&not_replay, pair), MakeResult::NoMark);
        assert!(matches!(make(&damaged, pair), MakeResult::Failed(_)));
        assert!(matches!(
            make(&rep.join("missing.rpy"), pair),
            MakeResult::Failed(_)
        ));
        let dirs: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(dirs, ["rep"], "断ったのにフォルダができた");
        let inner = std::fs::read_dir(&rep)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .count();
        assert_eq!(inner, 0, "断ったのに元のフォルダにフォルダができた");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn analyze_reads_names_side_and_values() {
        let d = temp_dir("analyze");
        let p = d.join("g.rpy");
        std::fs::write(&p, replay(1, 0, Side::P2, None, true)).unwrap();
        let Analysis::Fixable(f) = analyze(&p) else {
            panic!("{:?}", analyze(&p));
        };
        assert_eq!(f.side, Some(Side::P2));
        assert_eq!(f.names, ("Bob".to_string(), "Alice".to_string()));
        assert_eq!(f.current, (Kind::Charge, Kind::Slow));
        assert!(matches!(
            analyze(&d.join("none.rpy")),
            Analysis::Unreadable(_)
        ));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_batch_counts_each_reason_and_stays_in_each_replays_own_folder() {
        let d = temp_dir("batch");
        let top = d.join("top");
        let sub = top.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let a = top.join("a.rpy");
        let b = sub.join("b.rpy");
        let same = top.join("same.rpy");
        let done = top.join("done.rpy");
        std::fs::write(&a, replay(0, 0, Side::P1, None, true)).unwrap();
        std::fs::write(&b, replay(1, 1, Side::P2, None, true)).unwrap();
        std::fs::write(&same, replay(0, 1, Side::P1, None, true)).unwrap();
        let recorded = MatchDetail {
            charge: Some([0, 1]),
            ..Default::default()
        };
        std::fs::write(&done, replay(0, 1, Side::P1, Some(recorded), true)).unwrap();
        let r = make_all(
            &[a.clone(), b.clone(), same, done],
            (Kind::Slow, Kind::Charge),
        );
        let out_a = d.join("fix_replay").join("top").join("slow-charge");
        let out_b = top.join("fix_replay").join("sub").join("slow-charge");
        assert_eq!(r.made, vec![out_a.join("a.rpy"), out_b.join("b.rpy")]);
        assert_eq!(r.output_dirs(), vec![out_a, out_b]);
        assert_eq!((r.unchanged, r.recorded.len()), (1, 1));
        assert_eq!(r.recorded[0].0, 3, "対象の並びでの位置");
        assert_eq!((r.skipped(), r.total()), (2, 4));
        assert!(!r.has_problem());
        // もう一度なら、作ったものは「既にある」になる。
        let again = make_all(&[a, b], (Kind::Slow, Kind::Charge));
        assert_eq!((again.made.len(), again.exists), (0, 2));
        assert!(again.has_problem());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
