//! ゲーム内UIの入口の判定（pure。ゲームのメモリにも関数にも触らない）。
//!
//! 入口はMatchの画面（screen_id 6）の5番目「妖怪 対 妖怪」（カーソル4）の決定を借りる。
//! 決定を1回でもゲームに見せると取り返せない。
//! ゲームが内蔵ネット対戦の経路へ移る（`0x0042A834`〜`0x0042A856`）。
//! 入力の地点`0x0043112D`が呼ばれなくなる（`0x00431127`の分岐）。
//! オプションのフィールド`[0x4B3538]`が書かれて終了時にゲームの設定ファイルへ出る。
//! だから「このtickのワードをそのままfeedしたら決定が漏れるか」をゲームの処理の順そのままの模型で判定する（`leaks`）。
//! dllは漏れると判定したtickだけワードの代わりに0をfeedしてメニューを開く。
//! 模型の根拠（逆アセンブル）: ハンドラ`0x0042A7D1`（`[M+0x28]` = 1 の枝が`0x0042A809`）。
//! 画面の振り分け`0x0042A9CB`は`[M+0x1B238]`が0でないとハンドラを呼ばない（`0x0042A9CF`〜`0x0042A9DA`）。
//! `0x0042A80D`が先にカーソルの上下`0x0042457A`（n = 5）を呼ぶ。
//! **その後に** `[0x4ACF3A]`（InputManager[2]の押した瞬間`+0x06`）の下位16ビットに`0x1001`（ZとEnter）が立っていれば決定（`0x0042A81D`〜`0x0042A826`）。
//! `0x0042A828`〜`0x0042A82A`で動いた後のカーソルを読んで4と比べる。
//! ワードはdllが`feed 0x0042ACE0`で作らせる（`input_model::InputState::feed`が模型。実機12,568 tickで全件一致）。
//! そのためInputManager[2]の今の状態に今tickのワードをfeedした結果の`pressed` / `repeat`がゲームの読む値になる。
#![forbid(unsafe_code)]

// 上・下のビットは`input_model::keys`（ゲームの値: `0x00424592 push $0x10` / `0x004245C5 push $0x20`）。
use crate::input_model::InputState;
use crate::input_model::keys::{DOWN, UP, Z};
use crate::menu_model::{DECIDE_BITS, SCREEN_MODE};

pub const SCREEN_MATCH_MODE: u32 = SCREEN_MODE as u32;
/// Matchの画面の項目の数（`0x0042A809 push $5`）。
pub const MODE_ITEMS: i32 = 5;
/// 入口に借りる項目（5番目「妖怪 対 妖怪」。カーソルは0始まりで4。`0x0042A82A cmp $4`）。
pub const YOKAI_CURSOR: i32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryView {
    /// `[M+0x1B228]`。
    pub screen: u32,
    /// `[M+0x28]`（1のときだけ決定を読む）。
    pub sub: u32,
    /// `[M+0x1B238]`（0でなければハンドラが走らない）。
    pub trans: u32,
    /// `[M+0x00]`（符号つき32ビット。`0x004245B1 cmp %ebx,(%esi); jge`）。
    pub cursor: u32,
}

/// ゲームの上下の処理（`0x0042457A`）の後のカーソル。
/// `edge`はInputManager[2]の押した瞬間`+0x06`とリピート`+0x04`の論理和（どちらに立っていても`0x00423158`は真を返す）。
/// 上（`0x10`）を先に見て立っていれば下は見ない（`0x00424596`〜`0x004245A0`と`0x004245C5`〜`0x004245D3`）。
pub fn cursor_after_move(cursor: i32, edge: u16) -> i32 {
    cursor_after_move_n(cursor, edge, MODE_ITEMS)
}

/// `cursor_after_move`の項目の数`n`を選べる形（同じ`0x0042457A`をタイトルは n = 8 で呼ぶ —— `0x0042A0C4 push $8`）。
pub fn cursor_after_move_n(cursor: i32, edge: u16, n: i32) -> i32 {
    let step = |c: i32| {
        // 動いた後の折り返し: 負なら n − 1、n以上なら0（この順。`0x004245B1`〜`0x004245BE`・`0x004245E4`〜`0x004245F1`）。
        let c = if c < 0 { n - 1 } else { c };
        if c >= n { 0 } else { c }
    };
    if edge & UP != 0 {
        step(cursor.wrapping_sub(1))
    } else if edge & DOWN != 0 {
        step(cursor.wrapping_add(1))
    } else {
        cursor
    }
}

/// 今のtickにワード`word`をInputManager[2]へfeedしたときにゲームが読む`(押した瞬間, リピート)`。
/// `im2`はfeedの前のInputManager[2]の状態（`feeds::im_from_bytes`）。
pub fn predicted_edges(im2: &InputState, word: u16) -> (u16, u16) {
    let mut s = *im2;
    s.feed(word);
    (s.pressed, s.repeat)
}

/// このtickに`word`をそのままfeedしたら「妖怪 対 妖怪」の決定がゲームに読まれるかどうか。
/// 真なら呼び手はワードをfeedせず（0を3本）自前のメニューを開く。
///
/// 決定が読まれる条件（全部）: Matchの画面・`[M+0x28]` = 1・`[M+0x1B238]` = 0・押した瞬間に`0x1001`（ZかEnter。リピートは使われない）・同じtickの上下の処理を済ませた後のカーソルが4。
pub fn leaks(v: &EntryView, im2: &InputState, word: u16) -> bool {
    if v.screen != SCREEN_MATCH_MODE || v.sub != 1 || v.trans != 0 {
        return false;
    }
    let (pressed, repeat) = predicted_edges(im2, word);
    if pressed & DECIDE_BITS == 0 {
        return false;
    }
    cursor_after_move(v.cursor as i32, pressed | repeat) == YOKAI_CURSOR
}

/// 接続した後の自動の前進（両者が自動で難易度の画面まで進む）の1 tickの答え。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    /// 何も押さない（0をfeed）。
    Idle,
    /// このワードをfeedする。
    Press(u16),
}

/// 自動の前進の1 tick: Matchの画面のカーソルを押下で（メモリへは書かない）上へ動かす。
/// カーソルが0（「人間 対 人間」）に着いたのを読んでから決定を1回押す。
/// カーソル4のまま決定の押下を渡すと内蔵ネット対戦へ移って取り返せない。
/// だから決定の押下は必ず「いま読んだカーソルが0」のtickにだけ渡す。
///
/// - 同じキーを押したままのtick（`im2.cur`に立っている）は離す（`Idle`）。押した瞬間（0 → 1）を作らないと読まれない。
/// - `[M+0x28]`が1でない・`[M+0x1B238]`が0でないtickは何もしない（ゲームが読まない）。
/// - 最後に`leaks`で検証する。上の規則では起きない見込み。ただし1回の漏れが取り返せないので規則とは別に守る。
pub fn advance(v: &EntryView, im2: &InputState) -> Advance {
    // 呼び手（`net_ui`）は画面6のtickにしか呼ばない。念のため画面6でないなら何も押さない。
    if v.screen != SCREEN_MATCH_MODE || v.sub != 1 || v.trans != 0 {
        return Advance::Idle;
    }
    let want = if v.cursor as i32 == 0 { Z } else { UP };
    if im2.cur & want != 0 || leaks(v, im2, want) {
        return Advance::Idle;
    }
    Advance::Press(want)
}

#[cfg(test)]
mod tests {
    use super::*;

    const Z: u16 = 0x0001;
    const X: u16 = 0x0002;
    const ENTER: u16 = 0x1000;
    const DOWN: u16 = 0x0020;

    fn view(cursor: i32) -> EntryView {
        EntryView {
            screen: SCREEN_MATCH_MODE,
            sub: 1,
            trans: 0,
            cursor: cursor as u32,
        }
    }

    fn idle() -> InputState {
        InputState::new()
    }

    #[test]
    fn a_decide_on_the_yokai_item_leaks_and_on_any_other_item_does_not() {
        for word in [Z, ENTER, Z | ENTER] {
            assert!(leaks(&view(4), &idle(), word), "word {word:#06x}");
        }
        for c in [0, 1, 2, 3] {
            for word in [Z, ENTER] {
                assert!(
                    !leaks(&view(c), &idle(), word),
                    "cursor {c} word {word:#06x}"
                );
            }
        }
        for c in [5, 6, 100, -2] {
            assert!(!leaks(&view(c), &idle(), Z), "cursor {c}");
        }
    }

    #[test]
    fn the_move_in_the_same_tick_decides_which_item_the_decide_hits() {
        assert!(leaks(&view(3), &idle(), 0x0020 | Z));
        assert!(leaks(&view(0), &idle(), 0x0010 | Z));
        assert!(!leaks(&view(4), &idle(), 0x0020 | Z));
        assert!(!leaks(&view(4), &idle(), 0x0010 | Z));
        // 上と下が同時: 上だけが効く（`0x004245A0`の後に下を見ない）。4 → 3、3 → 2（4にならない）、0 → 4。
        assert!(!leaks(&view(4), &idle(), 0x0030 | Z));
        assert!(!leaks(&view(3), &idle(), 0x0030 | Z));
        assert!(leaks(&view(0), &idle(), 0x0030 | Z));
        assert!(!leaks(&view(3), &idle(), 0x0020));
        assert!(!leaks(&view(4), &idle(), 0));
    }

    #[test]
    fn only_a_tick_the_game_reads_the_decide_counts() {
        let ok = view(4);
        assert!(leaks(&ok, &idle(), Z));
        for bad in [
            EntryView { screen: 7, ..ok },
            EntryView { screen: 5, ..ok },
            EntryView { screen: 0, ..ok },
            EntryView { sub: 0, ..ok },
            EntryView { sub: 2, ..ok },
            EntryView { sub: 3, ..ok },
            EntryView { trans: 1, ..ok },
            EntryView { trans: 2, ..ok },
            EntryView {
                trans: 0x8000_0000,
                ..ok
            },
        ] {
            assert!(!leaks(&bad, &idle(), Z), "{bad:?}");
        }
    }

    #[test]
    fn only_a_fresh_z_or_enter_press_counts() {
        let mut held = idle();
        held.feed(Z);
        assert!(!leaks(&view(4), &held, Z));
        assert!(!leaks(&view(4), &held, Z | 0x0010));
        let mut released = held;
        released.feed(0);
        assert!(leaks(&view(4), &released, Z));
        assert!(leaks(&view(4), &held, Z | ENTER));
        for w in [
            X, 0x0004, 0x0008, 0x0100, 0x0200, 0x4000, 0x0010, 0x0020, 0x0040, 0x0080, 0x0800,
            0x2000,
        ] {
            assert!(!leaks(&view(4), &idle(), w), "word {w:#06x}");
        }
    }

    #[test]
    fn a_repeat_tick_moves_the_cursor_but_never_decides() {
        // 下を25 tick押し続けた状態。次のtick（26回目）でリピートが発火する（`input_model`の閾値）。
        let mut im = idle();
        for _ in 0..25 {
            im.feed(0x0020);
        }
        let (pressed, repeat) = predicted_edges(&im, 0x0020);
        assert_eq!((pressed, repeat), (0, 0x0020), "26 回目のリピート");
        // リピートで3 → 4に動く。同じtickにZを新しく押すと決定が「妖怪 対 妖怪」に当たる。
        assert!(leaks(&view(3), &im, 0x0020 | Z));
        assert!(!leaks(&view(3), &im, 0x0020));
        // リピートのtickでカーソルが4のまま決定の押下があっても、下のリピートで0へ動けば当たらない。
        assert!(!leaks(&view(4), &im, 0x0020 | Z));
        let mut zrep = idle();
        for _ in 0..25 {
            zrep.feed(Z);
        }
        let (p, r) = predicted_edges(&zrep, Z);
        assert_eq!((p, r), (0, Z));
        assert!(!leaks(&view(4), &zrep, Z));
    }

    #[test]
    fn the_cursor_model_matches_a_hand_counted_table() {
        // (今のカーソル, 上のあと, 下のあと)
        let table = [(0, 4, 1), (1, 0, 2), (2, 1, 3), (3, 2, 4), (4, 3, 0)];
        for (c, up, down) in table {
            assert_eq!(cursor_after_move(c, 0x0010), up, "up from {c}");
            assert_eq!(cursor_after_move(c, 0x0020), down, "down from {c}");
            assert_eq!(cursor_after_move(c, 0x0000), c, "no move from {c}");
            assert_eq!(cursor_after_move(c, 0x0040 | 0x0080 | Z), c, "left/right/Z");
            assert_eq!(cursor_after_move(c, 0x0030), up, "both = up");
        }
        // 範囲の外から動いたときもゲームの式のとおり（−1で上 → −2 → n − 1 = 4。5で下 → 6 → 0）。
        assert_eq!(cursor_after_move(-1, 0x0010), 4);
        assert_eq!(cursor_after_move(5, 0x0020), 0);
        assert_eq!(cursor_after_move(5, 0x0010), 4);
    }

    /// 自動の前進をゲームの模型で最後まで回す。戻りは（押したワードの列, 決定を押したときのカーソル, 漏れた回数）。`start`は初めのカーソル。
    fn run_advance(start: i32) -> (Vec<u16>, Option<i32>, u32) {
        let mut cursor = start;
        let mut im = InputState::new();
        let mut fed = Vec::new();
        let mut decided_at = None;
        let mut leaked = 0;
        for _ in 0..64 {
            let v = EntryView {
                screen: SCREEN_MATCH_MODE,
                sub: 1,
                trans: 0,
                cursor: cursor as u32,
            };
            let word = match advance(&v, &im) {
                Advance::Idle => 0,
                Advance::Press(w) => w,
            };
            if leaks(&v, &im, word) {
                leaked += 1;
            }
            let (pressed, repeat) = predicted_edges(&im, word);
            im.feed(word);
            fed.push(word);
            // ゲームの処理の順: 上下（動く）→ 決定（動いた後のカーソルを読む）。
            cursor = cursor_after_move(cursor, pressed | repeat);
            if pressed & 0x1001 != 0 {
                decided_at = Some(cursor);
                break;
            }
        }
        (fed, decided_at, leaked)
    }

    #[test]
    fn the_advance_walks_up_to_item_zero_then_decides_once_and_never_leaks() {
        for start in 0..5 {
            let (fed, decided_at, leaked) = run_advance(start);
            assert_eq!(leaked, 0, "start {start}: 漏れた");
            assert_eq!(
                decided_at,
                Some(0),
                "start {start}: 決定はカーソル 0 で（{fed:?}）"
            );
            assert_eq!(
                fed.iter().filter(|&&w| w == 0x0001).count(),
                1,
                "start {start}"
            );
            assert_eq!(
                fed.iter().filter(|&&w| w == UP).count(),
                start as usize,
                "start {start}"
            );
            // 同じワードを続けて渡さない（違うキーへ移るtickは前のキーが離れる）ので押した瞬間が毎回できる。
            for pair in fed.windows(2) {
                assert!(
                    pair[0] == 0 || pair[0] != pair[1],
                    "start {start}: 同じ語を続けて押している（押した瞬間ができない） {fed:?}"
                );
            }
        }
    }

    #[test]
    fn the_advance_waits_when_the_game_does_not_read_and_stops_off_the_screen() {
        let ok = EntryView {
            screen: SCREEN_MATCH_MODE,
            sub: 1,
            trans: 0,
            cursor: 2,
        };
        let idle = InputState::new();
        assert_eq!(advance(&ok, &idle), Advance::Press(UP));
        for bad in [
            EntryView { sub: 0, ..ok },
            EntryView { sub: 2, ..ok },
            EntryView { trans: 1, ..ok },
        ] {
            assert_eq!(advance(&bad, &idle), Advance::Idle, "{bad:?}");
        }
        for screen in [1, 5, 7, 8, 16] {
            assert_eq!(advance(&EntryView { screen, ..ok }, &idle), Advance::Idle);
        }
        let mut held = idle;
        held.feed(UP);
        assert_eq!(advance(&ok, &held), Advance::Idle);
        let at0 = EntryView { cursor: 0, ..ok };
        assert_eq!(advance(&at0, &idle), Advance::Press(Z));
        let mut zheld = idle;
        zheld.feed(Z);
        assert_eq!(advance(&at0, &zheld), Advance::Idle);
    }

    #[test]
    fn a_pressed_advance_word_never_leaks_in_any_state() {
        let states: [InputState; 8] = {
            let mut v = [InputState::new(); 8];
            for (i, w) in [0u16, UP, Z, UP | Z, DOWN, DOWN | Z, 0x0030, 0x1001]
                .into_iter()
                .enumerate()
            {
                let mut s = InputState::new();
                for _ in 0..(1 + i % 3) {
                    s.feed(w);
                }
                v[i] = s;
            }
            v
        };
        let (mut presses, mut z_presses, mut combos) = (0, 0, 0);
        for cursor in -3..9 {
            for screen in [1u32, 6, 7] {
                for sub in 0..3 {
                    for trans in 0..2 {
                        for im in &states {
                            combos += 1;
                            let v = EntryView {
                                screen,
                                sub,
                                trans,
                                cursor: cursor as u32,
                            };
                            if let Advance::Press(w) = advance(&v, im) {
                                presses += 1;
                                assert!(!leaks(&v, im, w), "{v:?} {w:#x}");
                                if w & 0x1001 != 0 {
                                    z_presses += 1;
                                    assert_eq!(cursor, 0, "決定はカーソル 0 のときだけ");
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(combos, 12 * 3 * 3 * 2 * 8);
        assert!(
            presses > 50 && z_presses > 0,
            "押した組 {presses}・決定 {z_presses}（0 なら検査が空）"
        );
    }
}
