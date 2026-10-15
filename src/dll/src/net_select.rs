//! ネットの選択で、選択の画面のゲームの状態から「自分の押下」「届いた押下をこの tick に入れられるか」
//! 「押下を入れる tick の feed とカーソル」を決める（pure。unsafe 無し）。押下の結果の模型と value の詰め方は
//! `higanr_core::menu_model`。ゲームのメモリの読み書きは呼び手。ここはゲームのスレッドで走るので、確保・
//! パニックしうる添字を置かない（添字はいつも `& 15` か `get`）。
#![forbid(unsafe_code)]

use higanr_core::input_model::keys;
use higanr_core::lockstep::by_seat;
use higanr_core::menu_model::{
    APPLY_CLEAR_BITS, MAP_ITEM_RANDOM_OF_TWO, MAP_LIST_EMPTY, SCREEN_CHARA, SCREEN_DIFFICULTY,
    SCREEN_MAP, TRANSITION_BITS, chara_value, chara_value_parts, is_selection_screen,
    peer_chara_word, press_kind,
};
use higanr_core::selection::{PeerCursor, Press};
use higanr_core::wire::{CURSOR_NONE, Seat, Step, StepKind};

use crate::th09_addr::{MENU_CHARA_CURSOR, MENU_CURSOR};

/// 入力の地点で読んだ選択の画面（今の場面 1 で、次の要求が 2 でないときだけ読む）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MenuView {
    /// `[M+0x1B228]`。
    pub screen: u32,
    /// `[M+0x28]`（1 のときだけ押下を読む）。
    pub sub: u32,
    /// `[M+0x1B238]`（0 でなければハンドラが走らない）。
    pub trans: u32,
    /// `[M+0x00]`（難易度・マップのカーソル）。
    pub cursor: u32,
    /// `[M+0x10]` / `[M+0x14]`（キャラのカーソル。1P・2P）。
    pub chara_cursor: [u32; 2],
    /// `[M+0x18]` / `[M+0x1C]`（席の旗。1P・2P）。
    pub decided: [u32; 2],
    /// `[0x4A7EA4]`（0 = 人対人のキャラ選択）。
    pub select_kind: u32,
    /// マップの一覧 `0x4AC8C4`。
    pub map_list: [u8; 16],
}

pub fn seat_index(s: Seat) -> usize {
    match s {
        Seat::P1 => 0,
        Seat::P2 => 1,
    }
}

/// ネットの選択をする画面か（人対人の Match の 7・8・16）。
pub fn in_selection(v: &MenuView) -> bool {
    v.select_kind == 0 && is_selection_screen(v.screen)
}

/// この tick のハンドラが押下を読む状態か（`+0x28 == 1` かつ `+0x1B238 == 0`）。
pub fn accepting(v: &MenuView) -> bool {
    in_selection(v) && v.sub == 1 && v.trans == 0
}

/// この tick に入れたら、ゲームがその押下を処理するか。キャラ選択の決定は、その席が決めていないときだけ。
/// 難易度の取消の `[[0x4B42D0]+0xA8] == 0` は見ない（普段は 0 の見込み）。
pub fn reads(v: &MenuView, kind: StepKind, value: u8) -> bool {
    if kind == StepKind::Format {
        // 形式の切替はゲームへ何も入れないので、ゲームが押下を読む状態を待たない（待つと、入る演出の間に届いた切替が
        // 持ち越されて、最初の INPUT より前に揃える順がずれうる）。選択の画面にいればよい。
        return in_selection(v);
    }
    if !accepting(v) {
        return false;
    }
    if v.screen == u32::from(SCREEN_CHARA) && kind == StepKind::Decide {
        let (seat, _, _) = chara_value_parts(value);
        return v.decided.get(seat) == Some(&0);
    }
    true
}

/// 届いた押下（ホスト: 受けたゲストの要求 / ゲスト: 届いた STEP）を、この tick に入れられるか。押下の画面が今の画面と
/// 同じで、ゲームがその押下を処理する状態のときだけ。画面を持たない押下（screen 0。peer_sim の v0）は入れない。
pub fn reads_step(v: &MenuView, st: &Step) -> bool {
    st.screen != 0 && u32::from(st.screen) == v.screen && reads(v, st.kind, st.value)
}

/// マップの一覧でカーソルが指す値（0〜15）。空き枠（0xFF）で決めるとゲームは −1 を書く（`0x004264BF` の movsbl）ので、
/// 14（−1 の項目）に数える。カーソルが一覧の外なら None。
fn map_item_at_cursor(v: &MenuView) -> Option<u8> {
    let item = v.map_list.get((v.cursor & 15) as usize).copied()?;
    Some(if item == MAP_LIST_EMPTY {
        MAP_ITEM_RANDOM_OF_TWO
    } else {
        item & 0x0F
    })
}

/// ホスト: 今の画面の合わせの押下（value は `menu_model::uses_align` の注）。合わせを使う画面（難易度・マップ）でなければ None。
pub fn align_press(v: &MenuView) -> Option<Press> {
    let screen = u8::try_from(v.screen).ok()?;
    let value = match screen {
        SCREEN_DIFFICULTY => (v.cursor & 3) as u8,
        SCREEN_MAP => map_item_at_cursor(v)?,
        _ => return None,
    };
    Some(Press {
        kind: StepKind::Align,
        value,
        screen,
    })
}

/// ホストが合わせをこの tick に出してよいか。ゲームがカーソルを初期化し終えた後（`+0x28 == 1`。8 → 7 は入場の回が
/// 次の tick なので、入った直後のカーソルは前の画面の値のまま）で、自分の語に方向・Ctrl・Shift が立っていないとき
/// （押し続けが合わせをまたぐと、ゲストの側でその方向が「押した瞬間」として 1 回余計に数えられ、カーソルが 1 つずれる）。
pub fn can_align_now(v: &MenuView, local: u16) -> bool {
    accepting(v) && local & APPLY_CLEAR_BITS == 0
}

/// 自分の押下（`pressed` は押した瞬間、`held` は押している間）。ゲームがこの tick に処理する押下だけを押下にする。
pub fn local_press(v: &MenuView, held: u16, pressed: u16, seat: usize) -> Option<Press> {
    let kind = press_kind(pressed)?;
    let screen = u8::try_from(v.screen).ok()?;
    let value = match screen {
        SCREEN_DIFFICULTY => (v.cursor & 3) as u8,
        SCREEN_MAP => map_item_at_cursor(v)?,
        SCREEN_CHARA => match kind {
            StepKind::Decide => {
                let cursor = v.chara_cursor.get(seat).copied().unwrap_or(0);
                chara_value(seat, held & keys::SHIFT != 0, (cursor & 15) as u8)
            }
            StepKind::Cancel => chara_value(seat, false, 0),
            // 人の押下は決定か取消だけ（合わせはホストの役が作る。形式の切替は Tab で、ゲームの語ではない）。
            StepKind::Align | StepKind::Format => return None,
        },
        _ => return None,
    };
    reads(v, kind, value).then_some(Press {
        kind,
        value,
        screen,
    })
}

/// 押し直す押下を、いまの画面から作り直す（value はカーソルの値なので、押した後に合わせでカーソルが動いていたら、古い
/// 値のまま入れると画面に見えているカーソルと違う値を決める）。作れなければ元のまま。キャラの決定は元の Shift を残す。
pub fn repress(v: &MenuView, p: Press, seat: usize) -> Press {
    let bit = press_bit(p.kind);
    let shift =
        p.screen == SCREEN_CHARA && p.kind == StepKind::Decide && chara_value_parts(p.value).1;
    let held = if shift { bit | keys::SHIFT } else { bit };
    local_press(v, held, bit, seat)
        .filter(|q| q.kind == p.kind && q.screen == p.screen)
        .unwrap_or(p)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyPlan {
    /// feed の前に MainMenu の欄（オフセット, 値）へ書くカーソル。
    pub cursor: Option<(u32, u32)>,
    pub words: [u16; 3],
    /// キャラ選択の決定で、カーソルを書いた後に立ち絵を追わせる席とカーソル。難易度・マップの画面・取消では `None`。
    pub portrait: Option<(usize, u32)>,
}

/// 押下の語（決定は Z、取消は X。合わせは画面を移る押下ではないので押さない）。
fn press_bit(kind: StepKind) -> u16 {
    match kind {
        StepKind::Decide => keys::Z,
        StepKind::Cancel => keys::X,
        StepKind::Align | StepKind::Format => 0,
    }
}

/// いつもの tick の 3 本（自分の席に自分の語、相手の席に相手の語、合成に OR）。画面を移るビットは両方の語から落とす。
pub fn normal_words(my_seat: Seat, local: u16, remote: u16) -> [u16; 3] {
    let (p1, p2) = by_seat(my_seat, local & !TRANSITION_BITS, remote & !TRANSITION_BITS);
    [p1, p2, p1 | p2]
}

/// 押下を入れる tick の feed とカーソル。押した席の語に方向・Ctrl・Shift が立っていると決定の前にカーソルが動くので落とす。
/// 決定のときだけカーソルを合わせる（難易度の取消でも合わせないのは、ゲームが取消でもカーソルを th09.cfg の難易度
/// `[0x4B3538]` へコピーするので、相手の値を自分の設定に残さないため）。
pub fn apply_plan(v: &MenuView, st: &Step, my_seat: Seat, local: u16, remote: u16) -> ApplyPlan {
    if st.kind == StepKind::Align {
        // 合わせは `align_plan`。ここへ来たら、ホストの形（書かない）にしておく。
        return align_plan(v, st, my_seat, local, remote, false);
    }
    if st.kind == StepKind::Format {
        // 形式の切替: 押下もカーソルも入れない（value は形式・D・auto の旗）。語はいつもの tick と同じで、step の番号だけが進む。
        return ApplyPlan {
            cursor: None,
            words: normal_words(my_seat, local, remote),
            portrait: None,
        };
    }
    let mut w = normal_words(my_seat, local, remote);
    let bit = press_bit(st.kind);
    let decide = st.kind == StepKind::Decide;
    let mut cursor = None;
    let mut portrait = None;
    match st.screen {
        SCREEN_DIFFICULTY | SCREEN_MAP => {
            // 7・16 は合成 `[2]` だけを読む。3 本とも方向・Ctrl・Shift を落とし、合成に押下を入れる。
            for x in &mut w {
                *x &= !APPLY_CLEAR_BITS;
            }
            w[2] = w[0] | w[1] | bit;
            if decide {
                cursor = if st.screen == SCREEN_DIFFICULTY {
                    Some((MENU_CURSOR, u32::from(st.value & 3)))
                } else {
                    // 一覧の値から添字を探す（添字は解放状況で両者ごとに違う）。見つからなければ合わせない（SELECT が揃える）。
                    v.map_list
                        .iter()
                        .position(|&x| x == st.value)
                        .map(|i| (MENU_CURSOR, i as u32))
                };
            }
        }
        SCREEN_CHARA => {
            let (seat, shift, cur) = chara_value_parts(st.value);
            let slot = &mut w[seat & 1];
            *slot &= !APPLY_CLEAR_BITS;
            *slot |= bit;
            if decide && shift {
                *slot |= keys::SHIFT;
            }
            w[2] = w[0] | w[1];
            if decide {
                cursor = Some((MENU_CHARA_CURSOR + 4 * (seat as u32 & 1), u32::from(cur)));
                portrait = Some((seat & 1, u32::from(cur)));
            }
        }
        _ => {}
    }
    ApplyPlan {
        cursor,
        words: w,
        portrait,
    }
}

/// 合わせを入れる tick の feed とカーソル。**ゲスト**は、カーソルをホストの値へ書き（マップは自分の一覧から値の添字を探す。
/// 無ければ書かない）、3 本とも方向・Ctrl・Shift を落とす（書いた値をこの tick に動かさない）。**ホスト**は自分の値なので
/// 何も書かず、語もそのまま（`is_guest` が偽）。キャラ選択の合わせは無い（届いても書かない。呼び手は書けなかった数に数える）。
pub fn align_plan(
    v: &MenuView,
    st: &Step,
    my_seat: Seat,
    local: u16,
    remote: u16,
    is_guest: bool,
) -> ApplyPlan {
    let mut w = normal_words(my_seat, local, remote);
    let mut plan = ApplyPlan {
        cursor: None,
        words: w,
        portrait: None,
    };
    if !is_guest {
        return plan;
    }
    for x in &mut w {
        *x &= !APPLY_CLEAR_BITS;
    }
    plan.words = w;
    match st.screen {
        SCREEN_DIFFICULTY => plan.cursor = Some((MENU_CURSOR, u32::from(st.value & 3))),
        SCREEN_MAP => {
            plan.cursor = v
                .map_list
                .iter()
                .position(|&x| x == st.value)
                .map(|i| (MENU_CURSOR, i as u32));
        }
        _ => {}
    }
    plan
}

/// キャラ選択の tick に、相手の席へ feed する相手の入力ワード（`menu_model::peer_chara_word`。カーソルは位置で書く）。ほかの画面はそのまま。
pub fn peer_word(screen: u32, remote: u16) -> u16 {
    if screen == u32::from(SCREEN_CHARA) {
        peer_chara_word(remote)
    } else {
        remote
    }
}

/// 自分の MENU に載せる、自分の画面と自分の席のキャラのカーソル（キャラ選択の外は `CURSOR_NONE`）。
pub fn own_cursor(v: &MenuView, seat: usize) -> (u8, u8) {
    let screen = u8::try_from(v.screen).unwrap_or(0);
    let cursor = match v.chara_cursor.get(seat) {
        Some(&c) if screen == SCREEN_CHARA && c <= 15 => c as u8,
        _ => CURSOR_NONE,
    };
    (screen, cursor)
}

/// 相手の席の位置の書き込み（`peer_sync`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerSync {
    /// 書く MainMenu の欄（オフセット, 値）。
    pub cursor: (u32, u32),
    /// 立ち絵を追わせる席とカーソル。
    pub portrait: (usize, u32),
    /// カーソルの音を鳴らす（相手のカーソルの移動。ランダム・入ったときの最初の位置では鳴らさない）。
    pub sound: bool,
}

/// キャラ選択で、この tick に相手の席のカーソルを相手の位置へ書くか（押下を入れない tick だけ。呼び手が見る）。ゲームが押下を読む状態で、
/// 相手の席が決めていない（決めた席のカーソルはゲームも動かさない）ときだけ見る。位置が同じなら書かない。
/// `take` は相手の位置を取る（取ると `moved` が消えるので、見る条件が揃ったときだけ呼ぶ）。
pub fn peer_sync(
    v: &MenuView,
    my: usize,
    take: impl FnOnce() -> Option<PeerCursor>,
) -> Option<PeerSync> {
    if v.screen != u32::from(SCREEN_CHARA) || !accepting(v) {
        return None;
    }
    let other = 1 - (my & 1);
    if v.decided.get(other) != Some(&0) {
        return None;
    }
    let pc = take()?;
    if pc.screen != SCREEN_CHARA || pc.cursor > 15 {
        return None;
    }
    let cursor = u32::from(pc.cursor);
    if v.chara_cursor.get(other) == Some(&cursor) {
        return None;
    }
    Some(PeerSync {
        cursor: (MENU_CHARA_CURSOR + 4 * other as u32, cursor),
        portrait: (other, cursor),
        sound: pc.moved,
    })
}

/// マップ −2（解放済みから 1 つ）を、ホストが具体の値に置き換える（ゲームの乱数は使わない）。一覧の中の 0〜13 の値
/// （手元で解放済みのマップ）から `seed % 件数` 番目。1 つも無ければ −1。返すのは SELECT のマップの欄（i8 の下位バイト）。
pub fn pick_any_map(list: &[u8; 16], seed: u16) -> u8 {
    let n = list.iter().filter(|&&x| x <= 13).count();
    if n == 0 {
        return (-1i8) as u8;
    }
    list.iter()
        .copied()
        .filter(|&x| x <= 13)
        .nth(usize::from(seed) % n)
        .unwrap_or((-1i8) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::menu_model::{CANCEL_BITS, DECIDE_BITS};

    #[test]
    fn picks_an_unlocked_map_for_random_any() {
        let list = [
            0, 3, 13, 14, 15, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        assert_eq!(pick_any_map(&list, 0), 0);
        assert_eq!(pick_any_map(&list, 1), 3);
        assert_eq!(pick_any_map(&list, 5), 13);
        let none = [
            14, 15, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF,
        ];
        assert_eq!(pick_any_map(&none, 9), 0xFF);
    }

    fn view(screen: u8) -> MenuView {
        MenuView {
            screen: u32::from(screen),
            sub: 1,
            trans: 0,
            cursor: 2,
            chara_cursor: [5, 9],
            decided: [0, 0],
            select_kind: 0,
            map_list: [0, 1, 3, 4, 5, 6, 7, 8, 10, 11, 9, 12, 13, 14, 15, 0xFF],
        }
    }

    fn st(screen: u8, kind: StepKind, value: u8) -> Step {
        Step {
            step: 4,
            kind,
            value,
            screen,
        }
    }

    const D: StepKind = StepKind::Decide;
    const C: StepKind = StepKind::Cancel;

    #[test]
    fn reads_only_when_the_handler_would() {
        let v = view(7);
        assert!(reads(&v, D, 0) && reads(&v, C, 0));
        for bad in [
            MenuView { sub: 0, ..v },
            MenuView { sub: 3, ..v },
            MenuView { trans: 1, ..v },
            MenuView {
                select_kind: 1,
                ..v
            },
            MenuView { screen: 6, ..v },
        ] {
            assert!(!reads(&bad, D, 0), "{bad:?}");
        }
        let mut c = view(8);
        c.decided = [1, 0];
        assert!(!reads(&c, D, chara_value(0, false, 3)), "決めた 1P の決定");
        assert!(reads(&c, D, chara_value(1, false, 3)));
        assert!(
            reads(&c, C, chara_value(0, false, 0)),
            "決めた席の取消は読む"
        );
        assert!(reads_step(&c, &st(8, D, chara_value(1, false, 3))));
        assert!(
            !reads_step(&view(8), &st(7, C, 0)),
            "画面が違う（取消は 8 でも読まれる押下）"
        );
        assert!(
            !reads_step(&c, &st(0, D, chara_value(1, false, 3))),
            "画面なし"
        );
    }

    #[test]
    fn local_press_values() {
        let v = view(7);
        assert_eq!(local_press(&v, DECIDE_BITS, 0, 0), None, "押した瞬間が無い");
        let p = local_press(&v, 0x0003, 0x0003, 0).unwrap();
        assert_eq!((p.kind, p.value, p.screen), (D, 2, 7));
        assert_eq!(
            local_press(&MenuView { cursor: 7, ..v }, 1, 1, 0)
                .unwrap()
                .value,
            3
        );
        let p = local_press(&view(16), 0x0008, 0x0008, 1).unwrap();
        assert_eq!(
            (p.kind, p.value, p.screen),
            (C, 3, 16),
            "一覧の 2 番目は値 3"
        );
        assert_eq!(
            local_press(
                &MenuView {
                    cursor: 15,
                    ..view(16)
                },
                1,
                1,
                0
            )
            .unwrap()
            .value,
            MAP_ITEM_RANDOM_OF_TWO,
            "空き枠"
        );
        let p = local_press(&view(8), keys::Z | keys::SHIFT, keys::Z, 1).unwrap();
        assert_eq!(chara_value_parts(p.value), (1, true, 9));
        let p = local_press(&view(8), keys::X | keys::SHIFT, keys::X, 0).unwrap();
        assert_eq!(chara_value_parts(p.value), (0, false, 0));
        let mut c = view(8);
        c.decided = [1, 0];
        assert_eq!(
            local_press(&c, 1, 1, 0),
            None,
            "決めた席の決定は押下にしない"
        );
        assert_eq!(local_press(&MenuView { sub: 0, ..view(7) }, 1, 1, 0), None);
        assert_eq!(press_kind(CANCEL_BITS), Some(C));
    }

    #[test]
    fn align_press_values_and_when_the_host_may_align() {
        let p = align_press(&view(7)).unwrap();
        assert_eq!((p.kind, p.value, p.screen), (StepKind::Align, 2, 7));
        assert_eq!(align_press(&view(8)), None, "キャラ選択は合わせない");
        let p = align_press(&view(16)).unwrap();
        assert_eq!(
            (p.value, p.screen),
            (3, 16),
            "一覧の 3 番目（添字 2）は値 3"
        );
        let last = MenuView {
            cursor: 15,
            ..view(16)
        };
        assert_eq!(align_press(&last).unwrap().value, MAP_ITEM_RANDOM_OF_TWO);
        assert!(align_press(&view(6)).is_none(), "選択の外");
        let v = view(8);
        assert!(can_align_now(&v, 0));
        assert!(can_align_now(&v, keys::Z), "決定のビットは語から落ちている");
        assert!(!can_align_now(&MenuView { sub: 0, ..v }, 0), "入る演出の間");
        assert!(!can_align_now(&MenuView { trans: 1, ..v }, 0));
        for bit in [keys::UP, keys::DOWN, keys::LEFT, keys::RIGHT, keys::SHIFT] {
            assert!(!can_align_now(&v, bit), "{bit:#x}");
        }
    }

    #[test]
    fn align_plan_writes_for_the_guest_only_and_cancel_never_writes() {
        let al = |screen: u8, value: u8| Step {
            step: 3,
            kind: StepKind::Align,
            value,
            screen,
        };
        let (local, remote) = (0x0050 | keys::SHIFT, 0x00A0);
        let p = align_plan(&view(7), &al(7, 3), Seat::P2, local, remote, true);
        assert_eq!(p.cursor, Some((MENU_CURSOR, 3)));
        assert_eq!(p.words, [0, 0, 0], "書く tick は動かさない");
        let p = align_plan(&view(16), &al(16, 9), Seat::P2, 0, 0, true);
        assert_eq!(p.cursor, Some((MENU_CURSOR, 10)));
        let p = align_plan(&view(16), &al(16, 2), Seat::P2, 0, 0, true);
        assert_eq!(p.cursor, None);
        let p = align_plan(&view(8), &al(8, 0x74), Seat::P2, local, remote, true);
        assert_eq!(
            (p.cursor, p.portrait),
            (None, None),
            "キャラ選択の合わせは書かない"
        );
        let p = align_plan(&view(7), &al(7, 3), Seat::P1, keys::Z, 0x0040, false);
        assert_eq!((p.cursor, p.portrait), (None, None));
        assert_eq!(p.words, normal_words(Seat::P1, keys::Z, 0x0040));
        for screen in [7u8, 8, 16] {
            let cancel = Step {
                step: 3,
                kind: StepKind::Cancel,
                value: 3,
                screen,
            };
            let p = apply_plan(&view(screen), &cancel, Seat::P2, 0, 0);
            assert_eq!((p.cursor, p.portrait), (None, None), "画面 {screen} の取消");
        }
    }

    #[test]
    fn repress_rebuilds_the_value_from_the_current_screen() {
        let old = Press {
            kind: StepKind::Decide,
            value: 3,
            screen: 7,
        };
        assert_eq!(repress(&view(7), old, 0).value, 2, "難易度のカーソルは 2");
        let old = Press {
            kind: StepKind::Decide,
            value: chara_value(1, true, 12),
            screen: 8,
        };
        assert_eq!(
            chara_value_parts(repress(&view(8), old, 1).value),
            (1, true, 9),
            "席と Shift は元のまま、カーソルは今の 9"
        );
        let cancel = Press {
            kind: StepKind::Cancel,
            value: 0,
            screen: 16,
        };
        assert_eq!(repress(&view(16), cancel, 0).value, 3);
        let old7 = Press {
            kind: StepKind::Decide,
            value: 3,
            screen: 7,
        };
        assert_eq!(
            repress(&MenuView { sub: 0, ..view(7) }, old7, 0),
            old7,
            "読まない間は作り直せない（元のまま）"
        );
    }

    #[test]
    fn apply_plans() {
        let n = normal_words(Seat::P2, 0x00A1 | keys::X, 0x0014 | keys::Z);
        assert_eq!(n, [0x0014, 0x00A0, 0x00B4]);
        let p = apply_plan(&view(7), &st(7, D, 3), Seat::P1, 0x0050, 0x0004);
        assert_eq!(p.cursor, Some((MENU_CURSOR, 3)));
        assert_eq!(p.words, [0, 0, keys::Z]);
        let p = apply_plan(&view(7), &st(7, C, 3), Seat::P1, 0x0001, 0);
        assert_eq!((p.cursor, p.words), (None, [0, 0, keys::X]));
        let p = apply_plan(&view(16), &st(16, D, 9), Seat::P1, 0, 0);
        assert_eq!(p.cursor, Some((MENU_CURSOR, 10)), "値 9 は一覧の 10 番目");
        let p = apply_plan(&view(16), &st(16, D, 2), Seat::P1, 0, 0);
        assert_eq!(p.cursor, None, "一覧に無い値は合わせない");
        let p = apply_plan(
            &view(8),
            &st(8, D, chara_value(1, true, 12)),
            Seat::P1,
            0x0040,
            0x0080 | keys::SHIFT,
        );
        assert_eq!(p.cursor, Some((MENU_CHARA_CURSOR + 4, 12)));
        assert_eq!(p.portrait, Some((1, 12)), "立ち絵を追わせる席とカーソル");
        assert_eq!(
            apply_plan(&view(7), &st(7, D, 3), Seat::P1, 0, 0).portrait,
            None,
            "難易度は立ち絵なし"
        );
        assert_eq!(
            apply_plan(&view(16), &st(16, D, 9), Seat::P1, 0, 0).portrait,
            None,
            "マップは立ち絵なし"
        );
        assert_eq!(
            p.words,
            [
                0x0040,
                keys::Z | keys::SHIFT,
                0x0040 | keys::Z | keys::SHIFT
            ]
        );
        let p = apply_plan(
            &view(8),
            &st(8, C, chara_value(0, false, 0)),
            Seat::P2,
            0x0010,
            0x0084,
        );
        assert_eq!(p.cursor, None);
        assert_eq!(p.portrait, None, "取消は立ち絵を追わせない");
        assert_eq!(p.words, [keys::X, 0x0010, 0x0010 | keys::X]);
    }

    fn pc(cursor: u8, moved: bool) -> Option<PeerCursor> {
        Some(PeerCursor {
            screen: 8,
            cursor,
            moved,
        })
    }

    /// 壊れたら落ちる: 自分の席へ書く・相手の席が決めた後も書く・押下を読まない間に書く・同じ位置へ書く・音の旗を落とす、
    /// 見ない条件のときに相手の位置を取ってしまう（`moved` が消えて音が鳴らない）。
    #[test]
    fn peer_sync_writes_the_other_seat_only_when_it_may_move() {
        let v = view(8);
        let s = peer_sync(&v, 0, || pc(12, true)).unwrap();
        assert_eq!(s.cursor, (MENU_CHARA_CURSOR + 4, 12));
        assert_eq!((s.portrait, s.sound), ((1, 12), true));
        let s = peer_sync(&v, 1, || pc(2, false)).unwrap();
        assert_eq!(s.cursor, (MENU_CHARA_CURSOR, 2));
        assert_eq!((s.portrait, s.sound), ((0, 2), false));
        assert_eq!(peer_sync(&v, 0, || pc(9, true)), None, "同じ位置");
        assert_eq!(peer_sync(&v, 0, || None), None);
        assert_eq!(
            peer_sync(&v, 0, || Some(PeerCursor {
                screen: 16,
                cursor: 3,
                moved: true
            })),
            None,
            "相手はキャラ選択の外"
        );
        let mut decided = v;
        decided.decided = [0, 1];
        for (bad, why) in [
            (decided, "相手の席が決めた"),
            (MenuView { sub: 0, ..v }, "入る演出"),
            (MenuView { trans: 1, ..v }, "移る途中"),
            (view(16), "画面 16"),
        ] {
            let mut taken = false;
            let got = peer_sync(&bad, 0, || {
                taken = true;
                pc(12, true)
            });
            assert_eq!((got, taken), (None, false), "{why}");
        }
        let mut mine = v;
        mine.decided = [1, 0];
        assert!(
            peer_sync(&mine, 0, || pc(12, true)).is_some(),
            "自分の席が決めていても書く"
        );
    }

    #[test]
    fn own_cursor_is_sent_only_on_the_chara_screen() {
        assert_eq!(own_cursor(&view(8), 0), (8, 5));
        assert_eq!(own_cursor(&view(8), 1), (8, 9));
        assert_eq!(own_cursor(&view(7), 0), (7, CURSOR_NONE));
        assert_eq!(own_cursor(&view(16), 1), (16, CURSOR_NONE));
        let mut v = view(8);
        v.chara_cursor = [16, 99];
        assert_eq!(own_cursor(&v, 0), (8, CURSOR_NONE), "範囲の外は送らない");
        assert_eq!(peer_word(8, keys::UP | keys::Z), keys::Z);
        assert_eq!(peer_word(7, keys::UP | keys::Z), keys::UP | keys::Z);
        assert_eq!(peer_word(16, 0x0100 | keys::DOWN), 0x0100 | keys::DOWN);
    }
}
