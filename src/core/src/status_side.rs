//! マッチ中の下の枠の通信状態の行を左右どちらに置くか（iniの`[ui] status_side`）。pure。
//!
//! 読み方は`lang`と同じ流儀: カテゴリかキーが無ければ既定（`1p` = 左下）。読めない値は断る。
//! 断った行を捨てて既定で動かすのは`ini_lenient`（`[ui]`の検査は`lang`とこの読みの両方を通す）。
#![forbid(unsafe_code)]

use crate::ini::Ini;
use crate::lang::SECTION;
use crate::wire::Seat;

pub const KEY: &str = "status_side";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatusSide {
    /// 左下に固定。
    #[default]
    P1,
    /// 右下に固定。
    P2,
    /// 自分側: 自分が1Pなら左下、2Pなら右下。
    Own,
    /// 相手側: 自分が1Pなら右下、2Pなら左下。
    Opponent,
}

impl StatusSide {
    pub const ALL: [StatusSide; 4] = [
        StatusSide::P1,
        StatusSide::P2,
        StatusSide::Own,
        StatusSide::Opponent,
    ];

    /// iniの語。
    pub const fn word(self) -> &'static str {
        match self {
            StatusSide::P1 => "1p",
            StatusSide::P2 => "2p",
            StatusSide::Own => "self",
            StatusSide::Opponent => "opponent",
        }
    }

    pub fn from_word(s: &str) -> Option<StatusSide> {
        let s = s.trim();
        StatusSide::ALL
            .into_iter()
            .find(|w| s.eq_ignore_ascii_case(w.word()))
    }

    /// 自分のサイドが`own`のとき右下に置くかどうか。
    pub const fn on_right(self, own: Seat) -> bool {
        match (self, own) {
            (StatusSide::P1, _) => false,
            (StatusSide::P2, _) => true,
            (StatusSide::Own, Seat::P2) | (StatusSide::Opponent, Seat::P1) => true,
            (StatusSide::Own, Seat::P1) | (StatusSide::Opponent, Seat::P2) => false,
        }
    }
}

/// `[ui] status_side`を読む。カテゴリかキーが無ければ`1p`。知らない値は断る（キーの綴りは`lang::setting_in_ini`が見る）。
pub fn from_ini(ini: &Ini) -> Result<StatusSide, String> {
    match ini.get(SECTION, KEY) {
        None => Ok(StatusSide::default()),
        Some(v) => StatusSide::from_word(v).ok_or_else(|| {
            format!(
                "[{SECTION}] {KEY} の値 {v:?} は {} のどれかにしてください",
                StatusSide::ALL.map(StatusSide::word).join(" / ")
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ini;

    fn read(text: &str) -> Result<StatusSide, String> {
        from_ini(&ini::parse(text).unwrap())
    }

    #[test]
    fn every_word_reads_back_and_the_default_is_the_left() {
        assert_eq!(read(""), Ok(StatusSide::P1));
        assert_eq!(read("[ui]\nlang = en\n"), Ok(StatusSide::P1));
        for s in StatusSide::ALL {
            assert_eq!(read(&format!("[ui]\nstatus_side = {}\n", s.word())), Ok(s));
            assert_eq!(StatusSide::from_word(s.word()), Some(s));
        }
        assert_eq!(read("[UI]\nStatus_Side = SELF \n"), Ok(StatusSide::Own));
        assert_eq!(StatusSide::default().word(), "1p");
    }

    #[test]
    fn unknown_values_are_refused_with_the_valid_words() {
        for bad in ["", "left", "right", "p1", "1", "own", "opponent2"] {
            let e = read(&format!("[ui]\nstatus_side = {bad}\n")).unwrap_err();
            assert!(e.contains("1p / 2p / self / opponent"), "{bad:?}: {e}");
        }
    }

    #[test]
    fn the_four_choices_put_the_row_where_the_decision_says() {
        use Seat::{P1, P2};
        let right = |s: StatusSide| [s.on_right(P1), s.on_right(P2)];
        assert_eq!(right(StatusSide::P1), [false, false], "1p は席によらず左");
        assert_eq!(right(StatusSide::P2), [true, true], "2p は席によらず右");
        assert_eq!(
            right(StatusSide::Own),
            [false, true],
            "自分側: 1P は左、2P は右"
        );
        assert_eq!(
            right(StatusSide::Opponent),
            [true, false],
            "相手側: 1P は右、2P は左"
        );
    }
}
