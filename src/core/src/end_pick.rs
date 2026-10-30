//! マッチ終了時のメニュー（`0x004355F0`）を1P・2Pが別々に選ぶステートマシン（pure）。
//! 2人のワードからメニューのキーを取り除き、各サイドの仮のカーソルと確定を持つ。
//! 決まったらゲームのカーソルを決まった項目へ動かして決定を`[2]`に差し込む。
//! 入力はサイド順の (p1, p2)と入力の地点で読んだメニューの状態だけ。
//! そのため2台と観戦者で同じ列から同じ結果になる。
//! 確保もパニックもしない（ゲームのスレッドで毎tick呼ぶ）。
#![forbid(unsafe_code)]

use crate::input_model::keys::{DOWN, UP, Z};
use crate::menu_model::DECIDE_BITS;
use crate::return_flow::{CLOSE_FRAMES, MENU_READ_FRAMES, end_menu};

pub mod model;

/// 終わりのメニューが`[2]`の押した瞬間から読むキー（上`0x004356FB push $0x10`・下`0x00435720 push $0x20`・決定`0x00435745 push $0x1001`）。
pub const MENU_KEYS: u16 = UP | DOWN | DECIDE_BITS;
/// 項目の番号（ゲームの状態1・2・3から1を引いた値）。下の項目ほど優先する。
pub const REMATCH_ITEM: u8 = 0;
pub const CHARA_ITEM: u8 = 1;
pub const SAVE_ITEM: u8 = 2;
const ITEMS: u8 = 3;
/// 決定の後の状態（4・5・6）は決定した項目の状態に3を足した値（`0x0043577F` / `0x004358CD` / `0x004359ED`）。
const DECIDED_BASE: u32 = end_menu::SAVE + 1;

/// サイドの印がある項目の色と、印の無い項目・決定の後の点滅で消えている間の色。
/// ゲーム自身の「選択中」と「非選択」の色（`0x00435684 movl $0xffffffff,0x1f8(%esi)` / `0x00435672 mov $0xff505050,%eax`）。
/// α はゲームのVMの値のまま使う。
pub const RGB_LIT: u32 = 0x00FF_FFFF;
pub const RGB_DIM: u32 = 0x0050_5050;
/// 決定の後の点滅の1区間のフレーム数。決定のtickから 消える → 点く → 消える → 点く の順（決定から次の場面まで`CLOSE_FRAMES`）。
pub const BLINK_FRAMES: u32 = 5;
const _: () = assert!(BLINK_FRAMES * 4 <= CLOSE_FRAMES);

/// 入力の地点で読んだ終わりのメニュー。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct View {
    /// `MATCH_END_MENU`（`0x4A7ECE`）が0でない。
    pub shown: bool,
    /// 一時停止のバイト（`0x4A7ECC`）が0でない。一時停止の間も終わりのメニューは更新される（`0x00435BB3` → `0x00435BCB`）。
    pub paused: bool,
    /// メニューの状態`+0x00`とフレームの数え`+0x04`（`shown`のときだけ意味がある）。
    pub state: u32,
    pub frames: u32,
}

/// サイドごとの縁取りの印。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mark {
    #[default]
    None,
    Picking(u8),
    Confirmed(u8),
}

/// 項目の文字の色の指示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Paint {
    /// ゲームの色のまま。
    #[default]
    Game,
    /// 選択中: どちらかのサイドの印がある項目は`RGB_LIT`。ほかは`RGB_DIM`。
    Choosing,
    /// 決定の後: この項目を点滅させる。ほかは`Choosing`と同じ。
    Decided(u8),
}

/// 画面に出すもの（縁取りの印と色の指示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Show {
    pub marks: [Mark; 2],
    pub paint: Paint,
}

impl Show {
    pub const NONE: Show = Show {
        marks: [Mark::None; 2],
        paint: Paint::Game,
    };
}

/// 鳴らす音の種類（番号はdllの`th09_addr`が持つ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Se {
    /// 仮のカーソルが動いた。
    Cursor,
    /// サイドが項目を確定した。
    Decide,
    /// 確定済みのサイドが上下で確定をとりやめた。
    Cancel,
}

/// 1 tickの出力。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Out {
    /// ゲームの`[2]`へfeedするワード（`[0]`・`[1]`はサイドのワードのまま）。
    pub word2: u16,
    pub show: Show,
    /// このtickに鳴らす音の数（確定のtickだけ）。
    /// カーソル: 仮のカーソルが動いたサイドの数（とりやめたサイドは数えない）。
    /// 決定: 確定したサイドの数（その確定でメニューが決まったtickは0。差し込んだ決定でゲーム自身が鳴らす）。
    /// とりやめ: 確定をとりやめたサイドの数。
    pub cursor_se: u8,
    pub decide_se: u8,
    pub cancel_se: u8,
    /// このtickに決まった項目（確定のtickだけ）。
    pub decided: Option<u8>,
    /// このtickにサイドごとに確定した項目（確定のtickだけ）。
    /// 影の動作の本ログが使う。影の動作ではゲームが最初の決定で閉じるので決まる前に終わる。
    pub confirms: [Option<u8>; 2],
}

/// 本ログへ渡すイベント。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    /// サイド`seat`（0 = 1P・1 = 2P）が項目を確定した。
    Confirmed {
        seat: u8,
        item: u8,
    },
    Decided(u8),
}

impl Out {
    /// このtickに鳴らす音を1つずつ。
    pub fn sounds(&self) -> impl Iterator<Item = Se> {
        let n = |k: u8, se| core::iter::repeat_n(se, usize::from(k));
        n(self.cursor_se, Se::Cursor)
            .chain(n(self.decide_se, Se::Decide))
            .chain(n(self.cancel_se, Se::Cancel))
    }

    /// 本ログへ渡すイベント（サイドごとの確定は影の動作のときだけ）。
    pub fn notes(&self, shadow: bool) -> impl Iterator<Item = Note> {
        let seats = self
            .confirms
            .into_iter()
            .enumerate()
            .filter_map(move |(s, c)| {
                c.filter(|_| shadow).map(|item| Note::Confirmed {
                    seat: s as u8,
                    item,
                })
            });
        seats.chain(self.decided.map(Note::Decided))
    }
}

/// ステートマシンの状態。`Copy`なのでrollbackのコピーにそのまま含められる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EndPick {
    /// サイドごとの前のtickのワード（押した瞬間を取るため。メニューの外でも毎tick更新する）。
    prev: [u16; 2],
    cursor: [u8; 2],
    confirmed: [Option<u8>; 2],
    decided: Option<u8>,
    /// 前のtickに`[2]`へ差し込んだキー（同じキーを続けて押すときは1 tick離す）。
    last_press: u16,
}

impl EndPick {
    pub const fn new() -> EndPick {
        EndPick {
            prev: [0; 2],
            cursor: [REMATCH_ITEM; 2],
            confirmed: [None; 2],
            decided: None,
            last_press: 0,
        }
    }

    /// 1 tick進める。`confirmed_tick`はそのtickの (p1, p2)が両者とも確定しているかどうか。
    /// rollbackの予測のtickは偽になる。偽のtickは差し込みを始めず音も知らせも出さない。
    pub fn step(&mut self, v: View, p1: u16, p2: u16, confirmed_tick: bool) -> Out {
        let edges = [p1 & !self.prev[0], p2 & !self.prev[1]];
        self.prev = [p1, p2];
        let merged = p1 | p2;
        if !v.shown {
            *self = EndPick {
                prev: self.prev,
                ..EndPick::new()
            };
            return Out {
                word2: merged,
                ..Out::default()
            };
        }
        // ゲームが人のキーを読む状態と同じ門（一時停止の間は扱わない）。数えは閉じた後も20のまま残るので状態と一緒に見る。
        let reading = !v.paused
            && (end_menu::REMATCH..=end_menu::SAVE).contains(&v.state)
            && v.frames >= MENU_READ_FRAMES;
        let (mut moved, mut decides, mut cancels) = (0u8, 0u8, 0u8);
        let mut decided_now = None;
        let mut confirms = [None; 2];
        if reading && self.decided.is_none() {
            for (seat, e) in edges.into_iter().enumerate() {
                let before = self.confirmed[seat];
                let m = self.seat_keys(seat, e);
                if e & DECIDE_BITS != 0 && before != self.confirmed[seat] {
                    confirms[seat] = self.confirmed[seat];
                    decides += 1;
                } else if m && before.is_some() {
                    cancels += 1;
                } else if m {
                    moved += 1;
                }
            }
            self.decided = self.verdict();
            decided_now = self.decided;
            if decided_now.is_some() {
                decides = 0;
            }
        }
        let mut press = 0;
        if let (true, true, Some(t)) = (reading, confirmed_tick, self.decided) {
            press = toward(v.state, t) & !self.last_press;
        }
        self.last_press = press;
        let paint = match v.state {
            s if (DECIDED_BASE..DECIDED_BASE + u32::from(ITEMS)).contains(&s) => {
                Paint::Decided((s - DECIDED_BASE) as u8)
            }
            _ => match self.decided {
                Some(t) if press & Z != 0 => Paint::Decided(t),
                _ => Paint::Choosing,
            },
        };
        let mark = |seat: usize| match self.confirmed[seat] {
            Some(i) => Mark::Confirmed(i),
            None => Mark::Picking(self.cursor[seat]),
        };
        Out {
            word2: (merged & !MENU_KEYS) | press,
            show: Show {
                marks: [mark(0), mark(1)],
                paint,
            },
            cursor_se: if confirmed_tick { moved } else { 0 },
            decide_se: if confirmed_tick { decides } else { 0 },
            cancel_se: if confirmed_tick { cancels } else { 0 },
            decided: decided_now.filter(|_| confirmed_tick),
            confirms: if confirmed_tick { confirms } else { [None; 2] },
        }
    }

    /// 1サイドの押した瞬間を扱う。ゲームと同じ順（上 → 下。下が後に書く。決定はそのtickの頭の項目）。仮のカーソルが動いたかどうかを返す。
    fn seat_keys(&mut self, seat: usize, e: u16) -> bool {
        let head = self.cursor[seat];
        if e & DECIDE_BITS != 0 {
            self.confirmed[seat] = Some(head);
            return false;
        }
        let mut c = head;
        if e & UP != 0 {
            c = (head + ITEMS - 1) % ITEMS;
        }
        if e & DOWN != 0 {
            c = (head + 1) % ITEMS;
        }
        if c == head {
            return false;
        }
        self.cursor[seat] = c;
        self.confirmed[seat] = None;
        true
    }

    /// どちらかが「リプレイを保存して終了」を確定していればその項目。2人とも確定していれば下の項目。
    fn verdict(&self) -> Option<u8> {
        if self.confirmed.contains(&Some(SAVE_ITEM)) {
            return Some(SAVE_ITEM);
        }
        match self.confirmed {
            [Some(a), Some(b)] => Some(a.max(b)),
            _ => None,
        }
    }
}

/// ゲームのカーソル（状態1〜3）から項目`t`へ向かう1手。着いていれば決定。項目は循環するのでどこからでも1手で着く。
fn toward(state: u32, t: u8) -> u16 {
    let s = (state - end_menu::REMATCH) as u8;
    if s == t {
        Z
    } else if t == (s + ITEMS - 1) % ITEMS {
        UP
    } else {
        DOWN
    }
}

/// 項目`item`の文字のRGB（`None`ならゲームの色のまま）。
/// `since`は`Paint::Decided`になってからのフレーム数。
/// `marked`はどちらかのサイドの印がその項目にあるかどうか。
pub fn item_rgb(paint: Paint, item: u8, since: u32, marked: bool) -> Option<u32> {
    match paint {
        Paint::Game => None,
        Paint::Decided(k) if k == item && (since / BLINK_FRAMES).is_multiple_of(2) => Some(RGB_DIM),
        Paint::Decided(k) if k == item => Some(RGB_LIT),
        _ if marked => Some(RGB_LIT),
        _ => Some(RGB_DIM),
    }
}

#[cfg(test)]
mod tests;
