//! マッチ終了時のメニュー`0x004355F0`のモデル（検査が使う。ゲームの処理の順のまま）。
//! 戻り道（`return_flow`）・ネット対戦のUI・選択のステートマシン（`end_pick`）の検査はどれもこの1つを使う。

use crate::input_model::keys::{DOWN, UP};
use crate::menu_model::DECIDE_BITS;
use crate::return_flow::{CLOSE_FRAMES, MENU_READ_FRAMES, end_menu};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EndMenu {
    /// `MATCH_END_MENU`（`0x4A7ECE`）。
    pub shown: bool,
    pub state: u32,
    pub frames: u32,
}

impl EndMenu {
    /// 出したばかりのメニュー（`0x004170A8` / `0x004178BE`が印を1にした後。数えは前の値が残っていてよい）。
    pub fn open(frames: u32) -> EndMenu {
        EndMenu {
            shown: true,
            state: end_menu::INIT,
            frames,
        }
    }

    /// 1フレーム（AsciiManagerのon_tick `0x00435BD4` → `0x4355F0`）。`pressed`は`[2]`の押した瞬間（`0x0040E2D0`は`+0x06`だけを読む）。
    /// 閉じたフレームに決定した項目（0 = 再戦・1 = キャラを変更・2 = リプレイを保存して終了）を返す。
    pub fn frame(&mut self, pressed: u16) -> Option<u8> {
        use end_menu::*;
        if !self.shown {
            return None;
        }
        if self.state == INIT {
            // 項目を作って状態を1に、数えを0にする。そのまま状態1の処理へ進む（`0x00435667`〜`0x00435670`）。
            self.state = REMATCH;
            self.frames = 0;
        }
        match self.state {
            REMATCH | CHARA | SAVE if self.frames >= MENU_READ_FRAMES => {
                let head = self.state;
                if pressed & UP != 0 {
                    self.state = [SAVE, REMATCH, CHARA][(head - REMATCH) as usize];
                }
                if pressed & DOWN != 0 {
                    self.state = [CHARA, SAVE, REMATCH][(head - REMATCH) as usize];
                }
                if pressed & DECIDE_BITS != 0 {
                    // 決定はそのtickの頭の状態で決まる（`0x0043577F` / `0x004358CD` / `0x004359ED`）。
                    self.state = head + 3;
                    self.frames = 0;
                }
            }
            4..=6 if self.frames >= CLOSE_FRAMES => {
                // 次の要求を書く。印を0にして戻る（数えは足さない。`0x00435A02`〜`0x00435A5E`）。
                let item = (self.state - 4) as u8;
                self.state = INIT;
                self.shown = false;
                return Some(item);
            }
            _ => {}
        }
        self.frames += 1;
        None
    }
}
