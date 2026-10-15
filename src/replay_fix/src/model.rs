//! 画面の状態。Win32 にもファイルにも触らない（窓は `win`、ファイルは `work` と `scan`）。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use higanr_core::replay::Side;
use higanr_core::replay_rewrite::{FixPlan, PlanError};

/// チャージ型。見出しの 1 バイト（0 = slow / 1 = charge。`th09.cfg` と同じ）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Slow,
    Charge,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Slow, Kind::Charge];

    pub fn from_u8(v: u8) -> Option<Kind> {
        match v {
            0 => Some(Kind::Slow),
            1 => Some(Kind::Charge),
            _ => None,
        }
    }

    pub fn value(self) -> u8 {
        match self {
            Kind::Slow => 0,
            Kind::Charge => 1,
        }
    }

    /// 画面とフォルダ名に出す語。ゲームの表記に合わせて英字のまま。
    pub fn name(self) -> &'static str {
        match self {
            Kind::Slow => "slow",
            Kind::Charge => "charge",
        }
    }

    pub fn from_name(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == s)
    }
}

/// (1P, 2P) の型。
pub type Pair = (Kind, Kind);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Player {
    P1,
    P2,
}

/// 出力先のフォルダ名。`<1P の型>-<2P の型>`（例 `slow-charge` は 1P = slow・2P = charge）。
pub fn pair_dir_name(pair: Pair) -> String {
    format!("{}-{}", pair.0.name(), pair.1.name())
}

/// `slow-charge` の形を読む（`pair_dir_name` の逆）。
pub fn parse_pair(s: &str) -> Option<Pair> {
    let (a, b) = s.split_once('-')?;
    Some((Kind::from_name(a)?, Kind::from_name(b)?))
}

/// 出力先のフォルダ名（4 通り）と同じ名前か。たどるときに読まない（作った写しをもう一度直すと戻ってしまう）。
pub fn is_pair_dir(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|s| parse_pair(&s.to_ascii_lowercase()).is_some())
}

/// 直した写しをまとめるフォルダの名前。
pub const FIX_DIR: &str = "fix_replay";

/// `fix_replay`（大文字小文字は区別しない）か。たどるときに読まない。
pub fn is_fix_dir(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|s| s.eq_ignore_ascii_case(FIX_DIR))
}

/// 直した写しの置き場所: `<元のフォルダの1つ上>\fix_replay\<元のフォルダ名>\<1P の型>-<2P の型>\<同じ名前>`。
/// 1つ上が無いとき（ドライブの直下など）は `<元のフォルダ>\fix_replay\<型>-<型>\<名前>`。
/// 元が `fix_replay` の下にあるときは `fix_replay\fix_replay` を作らず、元のフォルダの中に型のフォルダを作る。
/// どの場合も元のファイルとは別の場所になる。
pub fn output_path(src: &Path, pair: Pair) -> Option<PathBuf> {
    let name = src.file_name()?;
    let dir = src.parent().unwrap_or(Path::new(""));
    let under_fix = dir
        .ancestors()
        .any(|a| a.file_name().is_some_and(is_fix_dir));
    let root = match (under_fix, dir.parent(), dir.file_name()) {
        (false, Some(up), Some(leaf)) => up.join(FIX_DIR).join(leaf),
        (false, ..) => dir.join(FIX_DIR),
        (true, ..) => dir.to_path_buf(),
    };
    Some(root.join(pair_dir_name(pair)).join(name))
}

/// 1 本のリプレイを調べた結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Analysis {
    Fixable(Fixable),
    /// 印に `charge` がある（新しいバージョンで保存した／直し済み）。
    Recorded,
    /// HiganRTool が保存したリプレイではない（印が無い・読めない）。
    NoMark,
    /// 見出しの値が 0 / 1 のどちらでもない。
    OddValue(u8, u8),
    Unreadable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fixable {
    /// 保存した人の側。観戦者のリプレイは `None`。
    pub side: Option<Side>,
    /// (1P, 2P) の名前（表示用に整えたもの）。
    pub names: (String, String),
    pub current: Pair,
}

/// 名前は印から来る任意の文字列なので、制御文字を空白に替えて窓に出せる形にする。
fn clean_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

impl Analysis {
    pub fn from_plan(plan: Result<FixPlan, PlanError>) -> Analysis {
        match plan {
            Ok(p) => match (Kind::from_u8(p.current.0), Kind::from_u8(p.current.1)) {
                (Some(a), Some(b)) => Analysis::Fixable(Fixable {
                    side: p.side,
                    names: (clean_name(&p.names.0), clean_name(&p.names.1)),
                    current: (a, b),
                }),
                _ => Analysis::OddValue(p.current.0, p.current.1),
            },
            Err(PlanError::NoMark) => Analysis::NoMark,
            Err(PlanError::Recorded(_) | PlanError::Newer(_)) => Analysis::Recorded,
            Err(PlanError::Decode(e)) => Analysis::Unreadable(format!("{e:?}")),
        }
    }
}

/// 「リプレイ単体を修復」を押したときに起きること。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MakeState {
    /// 直せないリプレイ（ボタンを押せない）。
    Unavailable,
    /// 直す値が元の値と同じ。
    Unchanged,
    Ready(Pair),
}

/// 窓の下の知らせの行に出す内容（言語は出すときに決める）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    NothingFound,
    TooMany,
    Made(PathBuf),
    Unchanged,
    Exists(PathBuf),
    Failed(String),
}

#[derive(Debug)]
pub struct Entry {
    pub path: PathBuf,
    analysis: Option<Analysis>,
    pick: Option<Pair>,
}

/// 読み込んだリプレイの並びと、いま見ている 1 本。調べるのは見るときだけ（フォルダに数千本あっても開くのが遅くならない）。
#[derive(Debug)]
pub struct Session {
    entries: Vec<Entry>,
    index: usize,
}

impl Session {
    /// 空なら `None`。
    pub fn new(paths: Vec<PathBuf>) -> Option<Session> {
        if paths.is_empty() {
            return None;
        }
        Some(Session {
            entries: paths
                .into_iter()
                .map(|path| Entry {
                    path,
                    analysis: None,
                    pick: None,
                })
                .collect(),
            index: 0,
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 0 始まり。
    pub fn index(&self) -> usize {
        self.index
    }

    pub fn path(&self) -> &Path {
        &self.entries[self.index].path
    }

    pub fn can_prev(&self) -> bool {
        self.index > 0
    }

    pub fn can_next(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    pub fn prev(&mut self) {
        if self.can_prev() {
            self.index -= 1;
        }
    }

    pub fn next(&mut self) {
        if self.can_next() {
            self.index += 1;
        }
    }

    /// いまの 1 本をまだ調べていなければ `analyze` で調べる。最初の直す値は、リプレイに入っている値。
    pub fn ensure_analyzed(&mut self, analyze: impl FnOnce(&Path) -> Analysis) {
        let e = &mut self.entries[self.index];
        if e.analysis.is_none() {
            let a = analyze(&e.path);
            if let Analysis::Fixable(f) = &a {
                e.pick = Some(f.current);
            }
            e.analysis = Some(a);
        }
    }

    pub fn analysis(&self) -> Option<&Analysis> {
        self.entries[self.index].analysis.as_ref()
    }

    pub fn fixable(&self) -> Option<&Fixable> {
        match self.analysis() {
            Some(Analysis::Fixable(f)) => Some(f),
            _ => None,
        }
    }

    /// いまの 1 本の「直した後の設定」。直せないリプレイでは `None`。
    pub fn pick(&self) -> Option<Pair> {
        self.entries[self.index].pick
    }

    pub fn set_pick(&mut self, who: Player, kind: Kind) {
        if let Some(p) = &mut self.entries[self.index].pick {
            match who {
                Player::P1 => p.0 = kind,
                Player::P2 => p.1 = kind,
            }
        }
    }

    pub fn make_state(&self) -> MakeState {
        match (self.fixable(), self.pick()) {
            (Some(f), Some(p)) if f.current == p => MakeState::Unchanged,
            (Some(_), Some(p)) => MakeState::Ready(p),
            _ => MakeState::Unavailable,
        }
    }

    /// 「フォルダ全体のリプレイを同様に修復」の対象: いまのリプレイと同じフォルダにある、読み込み済みの全部。
    pub fn folder_targets(&self) -> Vec<PathBuf> {
        let dir = self.path().parent();
        self.entries
            .iter()
            .filter(|e| e.path.parent() == dir)
            .map(|e| e.path.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixable(current: Pair) -> Analysis {
        Analysis::Fixable(Fixable {
            side: Some(Side::P1),
            names: ("A".into(), "B".into()),
            current,
        })
    }

    fn session(paths: &[&str]) -> Session {
        Session::new(paths.iter().map(PathBuf::from).collect()).unwrap()
    }

    fn p(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    #[test]
    fn output_goes_to_fix_replay_beside_the_source_folder() {
        let src = p(&["rep", "Bob vs Alice", "th9_ud0008.rpy"]);
        let dst = output_path(&src, (Kind::Charge, Kind::Slow)).unwrap();
        assert_eq!(
            dst,
            p(&[
                "rep",
                "fix_replay",
                "Bob vs Alice",
                "charge-slow",
                "th9_ud0008.rpy"
            ])
        );
        // 1P と 2P を入れ替えると別のフォルダになる。
        assert_ne!(dst, output_path(&src, (Kind::Slow, Kind::Charge)).unwrap());
        assert_ne!(dst, src);
        assert_eq!(output_path(Path::new(""), (Kind::Slow, Kind::Slow)), None);
    }

    #[test]
    fn output_without_a_parent_folder_uses_fix_replay_inside_the_folder() {
        let pair = (Kind::Slow, Kind::Charge);
        // 元のフォルダに 1 つ上が無い（相対パスでファイル名だけ）。
        assert_eq!(
            output_path(Path::new("a.rpy"), pair),
            Some(p(&["fix_replay", "slow-charge", "a.rpy"]))
        );
        #[cfg(windows)]
        assert_eq!(
            output_path(Path::new("C:\\a.rpy"), pair),
            Some(p(&["C:\\", "fix_replay", "slow-charge", "a.rpy"]))
        );
        // 1 つ上が「空の親」でも、元のフォルダ名は残す。
        assert_eq!(
            output_path(&p(&["Alice", "a.rpy"]), pair),
            Some(p(&["fix_replay", "Alice", "slow-charge", "a.rpy"]))
        );
    }

    #[test]
    fn output_under_fix_replay_does_not_nest_another_fix_replay() {
        let pair = (Kind::Slow, Kind::Slow);
        for fix in ["fix_replay", "FIX_Replay"] {
            let direct = p(&["rep", fix, "a.rpy"]);
            assert_eq!(
                output_path(&direct, pair),
                Some(p(&["rep", fix, "slow-slow", "a.rpy"]))
            );
            let deeper = p(&["rep", fix, "Bob vs Alice", "charge-slow", "a.rpy"]);
            let got = output_path(&deeper, pair).unwrap();
            assert_eq!(
                got,
                p(&[
                    "rep",
                    fix,
                    "Bob vs Alice",
                    "charge-slow",
                    "slow-slow",
                    "a.rpy"
                ])
            );
            assert_eq!(
                got.components()
                    .filter(|c| is_fix_dir(c.as_os_str()))
                    .count(),
                1,
                "fix_replay が重なった"
            );
        }
        // 名前が似ているだけのフォルダは対象外。
        assert_eq!(
            output_path(&p(&["rep", "fix_replays", "a.rpy"]), pair),
            Some(p(&[
                "rep",
                "fix_replay",
                "fix_replays",
                "slow-slow",
                "a.rpy"
            ]))
        );
    }

    #[test]
    fn fix_replay_is_matched_ignoring_case_only_as_a_whole_name() {
        assert!(is_fix_dir(OsStr::new("fix_replay")));
        assert!(is_fix_dir(OsStr::new("Fix_Replay")));
        assert!(!is_fix_dir(OsStr::new("fix_replay2")));
        assert!(!is_fix_dir(OsStr::new("replay")));
    }

    #[test]
    fn pair_names_round_trip_and_only_the_four_are_pair_dirs() {
        for a in Kind::ALL {
            for b in Kind::ALL {
                let n = pair_dir_name((a, b));
                assert_eq!(parse_pair(&n), Some((a, b)));
                assert!(is_pair_dir(OsStr::new(&n)));
            }
        }
        assert_eq!(pair_dir_name((Kind::Slow, Kind::Charge)), "slow-charge");
        for bad in [
            "slow",
            "slow-",
            "slow-fast",
            "slow-charge-x",
            "Bob vs Alice",
        ] {
            assert!(!is_pair_dir(OsStr::new(bad)), "{bad}");
        }
    }

    #[test]
    fn the_first_pick_is_the_value_in_the_replay_and_making_the_same_values_is_refused() {
        let mut s = session(&["a/1.rpy"]);
        assert_eq!(s.make_state(), MakeState::Unavailable, "調べる前は押せない");
        s.ensure_analyzed(|_| fixable((Kind::Slow, Kind::Charge)));
        assert_eq!(s.pick(), Some((Kind::Slow, Kind::Charge)));
        assert_eq!(s.make_state(), MakeState::Unchanged);
        s.set_pick(Player::P2, Kind::Slow);
        assert_eq!(s.make_state(), MakeState::Ready((Kind::Slow, Kind::Slow)));
        s.set_pick(Player::P1, Kind::Charge);
        assert_eq!(s.make_state(), MakeState::Ready((Kind::Charge, Kind::Slow)));
    }

    #[test]
    fn replays_that_cannot_be_fixed_cannot_be_made() {
        for a in [
            Analysis::Recorded,
            Analysis::NoMark,
            Analysis::OddValue(2, 0),
            Analysis::Unreadable("x".into()),
        ] {
            let mut s = session(&["a/1.rpy"]);
            s.ensure_analyzed(|_| a.clone());
            assert_eq!(s.make_state(), MakeState::Unavailable, "{a:?}");
            assert_eq!(s.pick(), None);
            s.set_pick(Player::P1, Kind::Charge);
            assert_eq!(s.pick(), None);
        }
    }

    #[test]
    fn each_replay_keeps_its_own_pick_and_is_analyzed_once() {
        let mut s = session(&["a/1.rpy", "a/2.rpy"]);
        let mut calls = 0;
        s.ensure_analyzed(|_| {
            calls += 1;
            fixable((Kind::Slow, Kind::Slow))
        });
        s.set_pick(Player::P1, Kind::Charge);
        s.next();
        s.ensure_analyzed(|_| fixable((Kind::Charge, Kind::Charge)));
        assert_eq!(s.pick(), Some((Kind::Charge, Kind::Charge)));
        s.prev();
        s.ensure_analyzed(|_| {
            calls += 1;
            fixable((Kind::Slow, Kind::Slow))
        });
        assert_eq!(calls, 1, "もう一度調べた");
        assert_eq!(s.pick(), Some((Kind::Charge, Kind::Slow)));
    }

    #[test]
    fn prev_and_next_stop_at_the_ends() {
        let mut s = session(&["a/1.rpy", "a/2.rpy", "a/3.rpy"]);
        assert!(!s.can_prev() && s.can_next());
        s.prev();
        assert_eq!(s.index(), 0);
        s.next();
        s.next();
        assert!(s.can_prev() && !s.can_next());
        s.next();
        assert_eq!(s.index(), 2);
        assert!(Session::new(Vec::new()).is_none());
    }

    #[test]
    fn the_folder_batch_covers_only_the_loaded_replays_of_the_same_folder() {
        let mut s = session(&["x/a/1.rpy", "x/a/2.rpy", "x/b/3.rpy", "x/a/b/4.rpy"]);
        assert_eq!(
            s.folder_targets(),
            vec![PathBuf::from("x/a/1.rpy"), PathBuf::from("x/a/2.rpy")]
        );
        s.next();
        s.next();
        assert_eq!(s.folder_targets(), vec![PathBuf::from("x/b/3.rpy")]);
    }

    #[test]
    fn names_from_the_mark_are_cleaned_for_the_window() {
        let plan = FixPlan {
            side: None,
            names: ("A\tB\r\n".into(), " C ".into()),
            current: (0, 1),
        };
        let Analysis::Fixable(f) = Analysis::from_plan(Ok(plan)) else {
            panic!()
        };
        assert_eq!(f.names, ("A B".to_string(), "C".to_string()));
        assert_eq!(f.current, (Kind::Slow, Kind::Charge));
    }
}
