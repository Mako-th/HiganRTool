//! 花映塚の Match 人対人の選択の 3 画面（難易度 7・キャラ 8・マップ 16）で、画面を移る押下（決定・取消）がどう効くかの模型と、
//! STEP / STEP_REQ の `value` の詰め方。
//!
//! 使い手は 2 つ: dll（押下を読み、value を詰め、届いた押下が選択を出るものかを決める）と peer_sim（ゲームが無いので、
//! この模型で画面を追う）。
//!
//! 画面を移る押下は、ホストが順番を決めて両者のゲームへ同じ順に入れる（`selection.rs`）ので、両者の画面と席の旗は
//! 模型のとおりに同じに動く。模型が持たないもの: カーソルの移動（MENU の入力ワードで動く。キャラ選択の相手の席は MENU が運ぶ位置で
//! 書く。どちらも表示だけ）、入る演出（押下の後
//! 8〜9 tick は押下を読まない。dll はゲームのメモリの `+0x28` で見る）。

use crate::input_model::keys;
use crate::wire::{Step, StepKind};

/// 画面の ID（`[[0x4AC8C0]+0x1B228]`）。
pub const SCREEN_MODE: u8 = 6;
pub const SCREEN_DIFFICULTY: u8 = 7;
pub const SCREEN_CHARA: u8 = 8;
pub const SCREEN_MAP: u8 = 16;

/// 決定のビット（`+0x06 & 0x1001`。Z と Enter）。送る入力ワードは `MATCH_SEND_MASK` を掛けるので、ネットワークでは Z だけが残る。
pub const DECIDE_BITS: u16 = 0x1001;
/// 取消のビット（`+0x06 & 0x000A`。X と Esc）。
pub const CANCEL_BITS: u16 = 0x000A;
/// 画面を移るビット（MENU の入力ワードからは落とし、押下は STEP でだけ入れる）。
pub const TRANSITION_BITS: u16 = DECIDE_BITS | CANCEL_BITS;
/// 押下を入れるフレームに、押した席の入力ワードから落とすビット（方向・Shift・Ctrl）。カーソルの移動・「ランダム」・Shift は決定の
/// 判定より先に同じ tick で走るので、押下と同じ入力ワードに立っていると合わせたカーソルが決定の前に動く。キャラ選択の色は
/// value の Shift で入れ直す。
pub const APPLY_CLEAR_BITS: u16 = 0x00F0 | 0x0004 | 0x0100;

/// キャラ選択のカーソル（添字 0〜15）→ キャラ（表 `0x4A1E2C`）。
pub const CHARA_BY_CURSOR: [u8; 16] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x0E, 0x0F, 0x07, 0x08, 0x0A, 0x0B, 0x09, 0x0C, 0x0D,
];
/// マップの一覧の並び（表 `0x4A1D9C`。出す項目だけを前から詰めた一覧 `0x4AC8C4` の元）。14 = −1（2 人のどちらかの面）、
/// 15 = −2（解放済みから 1 つ）。
pub const MAP_ORDER: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 9, 12, 13, 14, 15];
/// 一覧の値 14・15 を決めたときにゲームが書くマップ（`0x004264D0` / `0x004264DE`）。
pub const MAP_ITEM_RANDOM_OF_TWO: u8 = 14;
pub const MAP_ITEM_RANDOM_ANY: u8 = 15;
/// マップの一覧の空き枠（`0x00426639`）。
pub const MAP_LIST_EMPTY: u8 = 0xFF;

/// 一覧の値（0〜15）を SELECT のマップ（i8 の下位バイト。−1 = 0xFF、−2 = 0xFE）にする。
pub fn map_select_byte(item: u8) -> u8 {
    match item {
        MAP_ITEM_RANDOM_OF_TWO => (-1i8) as u8,
        MAP_ITEM_RANDOM_ANY => (-2i8) as u8,
        v => v,
    }
}

/// 合わせ（`StepKind::Align`）を使う画面か（難易度 7・マップ 16）。合わせの value は、難易度 = カーソル 0〜3 / マップ = カーソルが
/// 指す一覧の値 0〜15（添字は解放状況で両者ごとに違うので使わない）。
///
/// キャラ選択（8）は使わない。相手の席のカーソルは MENU が運ぶ位置へ毎 tick 書く（`selection::PeerCursor`）。入力ワードで動かすと、
/// 未解放のキャラを飛ばす `0x0042304D` と「ランダム」の乱数が両者で違い、相手のカーソルが別のキャラへずれる。
pub fn uses_align(screen: u8) -> bool {
    screen == SCREEN_DIFFICULTY || screen == SCREEN_MAP
}

/// キャラ選択で、相手の席へ feed する相手の入力ワード。方向（`0x0042474A` が上下左右でカーソルを動かす）と「ランダム」の Ctrl は落とす
/// （カーソルは位置で書く）。Shift を押している間の左右は初期ライフの増減（`0x00428C63`〜。カーソルは動かない）なので残し、上下は落とす
/// （Shift を離した後もリピートで動きうる）。
pub fn peer_chara_word(word: u16) -> u16 {
    let keep = if word & keys::SHIFT != 0 {
        keys::LEFT | keys::RIGHT
    } else {
        0
    };
    word & !(keys::UP | keys::DOWN | KEY_CTRL | ((keys::LEFT | keys::RIGHT) & !keep))
}

/// キャラ選択の STEP の value: bit0 = 押した席（0 = 1P / 1 = 2P）、bit1 = Shift（色違い）、bit2〜5 = カーソル。
/// 取消ではカーソルを使わない。
pub fn chara_value(seat: usize, shift: bool, cursor: u8) -> u8 {
    (seat as u8 & 1) | (u8::from(shift) << 1) | ((cursor & 0x0F) << 2)
}

/// `chara_value` の逆（席の添字, Shift, カーソル）。
pub fn chara_value_parts(v: u8) -> (usize, bool, u8) {
    (usize::from(v & 1), v & 2 != 0, (v >> 2) & 0x0F)
}

/// 画面を移る押下の種類（押した瞬間の入力ワードから）。同じ tick に両方が立つと決定が先に処理される。
pub fn press_kind(pressed: u16) -> Option<StepKind> {
    if pressed & DECIDE_BITS != 0 {
        Some(StepKind::Decide)
    } else if pressed & CANCEL_BITS != 0 {
        Some(StepKind::Cancel)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 選択の画面に残る。
    Stay,
    /// 難易度の画面の取消で、選択の外（画面 6）へ出た。
    LeftToMode,
    MapDecided,
}

/// 押下が効かなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mismatch {
    /// 押下の画面（`Step::screen`）が模型の画面と違う。
    Screen { model: u8, step: u8 },
    /// 決めた席の決定（ゲームは読み捨てる。ホストは押下にしないので来ない）。
    AlreadyDecided { seat: usize },
    /// 選択の画面の外。
    Outside,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Decided {
    pub chara: u8,
    pub color: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    screen: u8,
    /// 席ごとの「決めた」（`+0x18` / `+0x1C`）と、決めたときの値。
    decided: [Option<Decided>; 2],
    difficulty: Option<u8>,
    map_item: Option<u8>,
}

impl Model {
    /// `screen` の画面に入った（最初の選択は難易度 7、マッチから戻ると押下なしでキャラ選択 8 まで進む）。
    pub fn new(screen: u8) -> Self {
        Model {
            screen,
            decided: [None; 2],
            difficulty: None,
            map_item: None,
        }
    }

    pub fn screen(&self) -> u8 {
        self.screen
    }

    pub fn decided(&self, seat: usize) -> Option<Decided> {
        self.decided.get(seat).copied().flatten()
    }

    pub fn difficulty(&self) -> Option<u8> {
        self.difficulty
    }

    pub fn map_item(&self) -> Option<u8> {
        self.map_item
    }

    fn go(&mut self, screen: u8) {
        self.screen = screen;
        if screen == SCREEN_CHARA {
            // キャラ選択の入場の回が両席の旗を 0 にする（`0x0042921A` / `0x0042921D`）。
            self.decided = [None; 2];
        }
    }

    /// 押下を 1 つ入れる。`step.screen` が 0 でなければ模型の画面と比べる。
    pub fn apply(&mut self, st: &Step) -> Result<Outcome, Mismatch> {
        if st.screen != 0 && st.screen != self.screen {
            return Err(Mismatch::Screen {
                model: self.screen,
                step: st.screen,
            });
        }
        match (self.screen, st.kind) {
            // 合わせ: 模型はカーソルを持たないので何も変わらない。
            (s, StepKind::Align) if is_selection_screen(u32::from(s)) => Ok(Outcome::Stay),
            // 対戦の形式の切替（キャラセレクトの間だけ）。ネットワークの側の形式が替わるだけで、ゲームの画面にも模型にも何も起きない。
            // ほかの画面の Format は食い違いにする。
            (SCREEN_CHARA, StepKind::Format) => Ok(Outcome::Stay),
            (SCREEN_DIFFICULTY, StepKind::Decide) => {
                // 7 → 8（`0x0042A500` / `0x0042A51C`）。
                self.difficulty = Some(st.value & 3);
                self.go(SCREEN_CHARA);
                Ok(Outcome::Stay)
            }
            (SCREEN_DIFFICULTY, StepKind::Cancel) => {
                // 7 → 6（`0x0042A60D`〜）。
                self.go(SCREEN_MODE);
                Ok(Outcome::LeftToMode)
            }
            (SCREEN_CHARA, StepKind::Decide) => {
                let (seat, shift, cursor) = chara_value_parts(st.value);
                let other = 1 - seat;
                if self.decided[seat].is_some() {
                    return Err(Mismatch::AlreadyDecided { seat });
                }
                let chara = CHARA_BY_CURSOR[usize::from(cursor)];
                // 色は Shift。相手が先に同じキャラで決めていれば 1 − 相手の色（`0x00428DE0`〜`0x00428E03`、
                // `0x00428E91`〜`0x00428EB6`）。
                let color = match self.decided[other] {
                    Some(o) if o.chara == chara => 1 - o.color,
                    _ => u8::from(shift),
                };
                self.decided[seat] = Some(Decided { chara, color });
                if self.decided[other].is_some() {
                    // 両席が揃った tick に 16 へ（`0x00428EBB`〜`0x00428F03`）。
                    self.go(SCREEN_MAP);
                }
                Ok(Outcome::Stay)
            }
            (SCREEN_CHARA, StepKind::Cancel) => {
                let (seat, _, _) = chara_value_parts(st.value);
                if self.decided[seat].is_some() {
                    // 決めた席の取消は取り消し（`0x00428F60`〜 / `0x00428FD1`〜）。
                    self.decided[seat] = None;
                } else {
                    // 選んでいる席の取消は難易度へ（`0x00428F54`。1P でも 2P でも）。
                    self.go(SCREEN_DIFFICULTY);
                }
                Ok(Outcome::Stay)
            }
            (SCREEN_MAP, StepKind::Decide) => {
                self.map_item = Some(st.value & 0x0F);
                Ok(Outcome::MapDecided)
            }
            (SCREEN_MAP, StepKind::Cancel) => {
                // 16 → 8（`0x00426518` / `0x00426541`）。入場の回で両席とも「選んでいる」に戻る。
                self.go(SCREEN_CHARA);
                Ok(Outcome::Stay)
            }
            _ => Err(Mismatch::Outside),
        }
    }
}

/// 押下が選択を出るものか（dll が、MENU を作る前に leave するかを決める）。画面と種類だけで決まる。
pub fn outcome_of(screen: u8, kind: StepKind) -> Outcome {
    match (screen, kind) {
        (SCREEN_DIFFICULTY, StepKind::Cancel) => Outcome::LeftToMode,
        (SCREEN_MAP, StepKind::Decide) => Outcome::MapDecided,
        _ => Outcome::Stay,
    }
}

pub fn is_selection_screen(screen: u32) -> bool {
    screen == u32::from(SCREEN_DIFFICULTY)
        || screen == u32::from(SCREEN_CHARA)
        || screen == u32::from(SCREEN_MAP)
}

/// 選択の画面で効く「ランダム」のキー（Ctrl）。キャラ選択 `0x00428B48`（1P は `[0]+0x00`）・`0x00428CC6`（2P は `[1]`）と
/// マップ `0x0042638B`（`[2]`）が、押している間に毎フレーム乱数でカーソルを回す（パッドのスキップのボタンも同じビット）。
pub const KEY_CTRL: u16 = keys::CTRL;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_model::MATCH_SEND_MASK;

    const D: StepKind = StepKind::Decide;
    const C: StepKind = StepKind::Cancel;

    #[test]
    fn the_network_word_mask_keeps_ctrl_and_drops_r_and_the_other_high_bits() {
        assert_ne!(
            MATCH_SEND_MASK & KEY_CTRL,
            0,
            "ネットワークの語は Ctrl を送る（ランダム）"
        );
        assert_eq!(
            MATCH_SEND_MASK, 0x01FF,
            "Enter など Ctrl 以外の上位ビットは送らない"
        );
        assert_eq!(MATCH_SEND_MASK & 0x4000, 0, "R は送らない");
        assert_eq!(MATCH_SEND_MASK & TRANSITION_BITS, TRANSITION_BITS & 0x00FF);
    }

    fn st(screen: u8, kind: StepKind, value: u8) -> Step {
        Step {
            step: 0,
            kind,
            value,
            screen,
        }
    }

    #[test]
    fn follows_the_screen_table() {
        let mut m = Model::new(SCREEN_DIFFICULTY);
        assert_eq!(m.apply(&st(7, D, 3)), Ok(Outcome::Stay));
        assert_eq!((m.screen(), m.difficulty()), (8, Some(3)));
        assert_eq!(
            m.apply(&st(8, D, chara_value(0, false, 2))),
            Ok(Outcome::Stay)
        );
        assert_eq!(
            m.apply(&st(8, C, chara_value(0, false, 0))),
            Ok(Outcome::Stay)
        );
        assert_eq!((m.screen(), m.decided(0)), (8, None));
        m.apply(&st(8, D, chara_value(0, true, 7))).unwrap();
        assert_eq!(m.screen(), 8, "片方だけでは 16 へ行かない");
        m.apply(&st(8, D, chara_value(1, false, 9))).unwrap();
        assert_eq!(m.screen(), 16);
        assert_eq!(
            m.decided(0),
            Some(Decided {
                chara: 0x0E,
                color: 1
            })
        );
        assert_eq!(m.decided(1).map(|d| d.chara), Some(0x07));
        assert_eq!(m.apply(&st(16, C, 0)), Ok(Outcome::Stay));
        assert_eq!((m.screen(), m.decided(0), m.decided(1)), (8, None, None));
        m.apply(&st(8, C, chara_value(1, false, 0))).unwrap();
        assert_eq!(m.screen(), 7);
        assert_eq!(m.apply(&st(7, C, 1)), Ok(Outcome::LeftToMode));
        assert_eq!(m.screen(), SCREEN_MODE);
        assert_eq!(m.apply(&st(0, D, 0)), Err(Mismatch::Outside));
        let mut m = Model::new(SCREEN_MAP);
        assert_eq!(m.apply(&st(16, D, 15)), Ok(Outcome::MapDecided));
        assert_eq!(m.map_item(), Some(15));
    }

    #[test]
    fn same_chara_takes_the_other_color() {
        let mut m = Model::new(SCREEN_CHARA);
        m.apply(&st(8, D, chara_value(1, false, 4))).unwrap();
        m.apply(&st(8, D, chara_value(0, false, 4))).unwrap();
        assert_eq!(m.decided(1).map(|d| d.color), Some(0));
        assert_eq!(m.decided(0).map(|d| d.color), Some(1), "同じキャラ");
        let mut m = Model::new(SCREEN_CHARA);
        m.apply(&st(8, D, chara_value(1, true, 4))).unwrap();
        m.apply(&st(8, D, chara_value(0, true, 4))).unwrap();
        assert_eq!(m.decided(0).map(|d| d.color), Some(0), "1 − 1");
        let mut m = Model::new(SCREEN_CHARA);
        m.apply(&st(8, D, chara_value(1, true, 4))).unwrap();
        m.apply(&st(8, D, chara_value(0, true, 5))).unwrap();
        assert_eq!(m.decided(0).map(|d| d.color), Some(1), "違うキャラは Shift");
    }

    #[test]
    fn align_changes_nothing_in_the_model() {
        let al = |screen: u8, value: u8| Step {
            step: 0,
            kind: StepKind::Align,
            value,
            screen,
        };
        for screen in [SCREEN_DIFFICULTY, SCREEN_CHARA, SCREEN_MAP] {
            let mut m = Model::new(screen);
            let before = m;
            assert_eq!(m.apply(&al(screen, 0x64)), Ok(Outcome::Stay));
            assert_eq!(m, before, "画面 {screen}");
        }
        let mut m = Model::new(SCREEN_MODE);
        assert_eq!(m.apply(&al(0, 0)), Err(Mismatch::Outside));
        let mut m = Model::new(SCREEN_CHARA);
        assert_eq!(
            m.apply(&al(SCREEN_MAP, 14)),
            Err(Mismatch::Screen { model: 8, step: 16 })
        );
        assert_eq!(outcome_of(SCREEN_MAP, StepKind::Align), Outcome::Stay);
        assert!(uses_align(SCREEN_DIFFICULTY) && uses_align(SCREEN_MAP));
        assert!(!uses_align(SCREEN_CHARA) && !uses_align(SCREEN_MODE) && !uses_align(0));
    }

    /// 壊れたら落ちる: 相手の語の方向・Ctrl を残す（カーソルが入力ワードでも動き、書いた位置からずれる）・Shift 付きの左右を
    /// 落とす（相手の初期ライフの表示が動かない）・Shift 付きの上下を残す。
    #[test]
    fn peer_chara_word_keeps_only_buttons_and_shift_left_right() {
        use crate::input_model::keys::{DOWN, LEFT, RIGHT, SHIFT, UP, X, Z};
        assert_eq!(peer_chara_word(UP | DOWN | LEFT | RIGHT | KEY_CTRL), 0);
        assert_eq!(peer_chara_word(Z | X | LEFT | KEY_CTRL), Z | X);
        assert_eq!(
            peer_chara_word(SHIFT | LEFT | UP | DOWN | KEY_CTRL),
            SHIFT | LEFT
        );
        assert_eq!(peer_chara_word(SHIFT | RIGHT | UP), SHIFT | RIGHT);
        assert_eq!(peer_chara_word(SHIFT | DOWN), SHIFT);
        for w in 0u16..=0xFFFF {
            let out = peer_chara_word(w);
            assert_eq!(out & !w, 0, "{w:#06x}: 無いビットを足した");
            assert_eq!(out & (UP | DOWN | KEY_CTRL), 0, "{w:#06x}");
            let lr = if w & SHIFT != 0 {
                w & (LEFT | RIGHT)
            } else {
                0
            };
            assert_eq!(out & (LEFT | RIGHT), lr, "{w:#06x}");
            assert_eq!(
                out & !(UP | DOWN | LEFT | RIGHT | KEY_CTRL),
                w & !(UP | DOWN | LEFT | RIGHT | KEY_CTRL),
                "{w:#06x}: ほかのビット"
            );
        }
    }

    #[test]
    fn refuses_mismatched_screens() {
        let mut m = Model::new(SCREEN_CHARA);
        assert_eq!(
            m.apply(&st(7, D, 0)),
            Err(Mismatch::Screen { model: 8, step: 7 })
        );
        assert_eq!(m.screen(), 8);
        m.apply(&st(0, D, chara_value(0, false, 1))).unwrap();
        assert_eq!(
            m.apply(&st(8, D, chara_value(0, false, 1))),
            Err(Mismatch::AlreadyDecided { seat: 0 })
        );
    }

    #[test]
    fn values_and_kinds() {
        for seat in 0..2 {
            for shift in [false, true] {
                for cursor in 0..16 {
                    let v = chara_value(seat, shift, cursor);
                    assert_eq!(chara_value_parts(v), (seat, shift, cursor));
                }
            }
        }
        assert_eq!(chara_value(1, true, 15), 0x3F);
        assert_eq!(map_select_byte(14), 0xFF);
        assert_eq!(map_select_byte(15), 0xFE);
        assert_eq!(map_select_byte(9), 9);
        assert_eq!(press_kind(0x0001 | 0x0002), Some(D));
        assert_eq!(press_kind(0x1000), Some(D));
        assert_eq!(press_kind(0x0008), Some(C));
        assert_eq!(press_kind(0x0004 | 0x00F0), None);
        assert_eq!(outcome_of(7, C), Outcome::LeftToMode);
        assert_eq!(outcome_of(16, D), Outcome::MapDecided);
        assert_eq!(outcome_of(16, C), Outcome::Stay);
        assert_eq!(outcome_of(8, C), Outcome::Stay);
        assert_eq!(outcome_of(7, D), Outcome::Stay);
        assert!(is_selection_screen(16) && !is_selection_screen(6) && !is_selection_screen(1));
        let mut seen = [false; 16];
        for c in CHARA_BY_CURSOR.iter().chain(MAP_ORDER.iter()) {
            seen[usize::from(*c)] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }
}
