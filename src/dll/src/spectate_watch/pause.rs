//! 観戦者のマッチの中の一時停止（pure）。Esc で手元のゲームのポーズ（`0x00434740`、this = `0x004D66F8`）を開き、ポーズのメニューを
//! 観戦者の手元の入力で動かす。開けない間（ホストのポーズの再現中・マッチの終わりのメニュー・前の語に Esc が入っている・この seg で崩れた）は、
//! 記録を止めた描画だけのフレーム（重ね描きの一時停止）にする。
//!
//! ポーズの間は試合のノードが走らない（GameManager が `0x0041AC1E` で走査を止める）ので記録を使わない。ポーズのメニューが読むのは `[2]+0x06`
//! だけなので `[2]` だけを feed し、`[0]`・`[1]` には触らない（入力の地点が poll を呼ばずに戻るので、どの InputManager もゲームは書かない。
//! 毎 tick 入れるとキーごとのリピートの数えがホストより進む）。再開の後に閉じる tick（状態 3 で数えが `CLOSE_FRAMES` 以上）に次の `Frame` を
//! 使う。その tick の GameManager は `[2]+0x06 & 8` を見るので、状態 3 の間の `[2]` は最後の `Frame` の `p1|p2` にする。
#![forbid(unsafe_code)]

use higanr_core::input_model::MATCH_SEND_MASK;
use higanr_core::input_model::keys::ESC;
use higanr_core::return_flow::{CLOSE_FRAMES, PlayView, pause};
use higanr_core::wire::Seat;

use super::WatchPlay;
use crate::netplay::peer_wait::ESC_HOLD_MS;
use crate::netplay::{Action, Game};

/// マッチの外の長押しの案内を、Esc を離してから残す長さ（ms。押す長さと同じ）。
pub const ESC_HINT_KEEP_MS: u32 = ESC_HOLD_MS;

/// ゲームのポーズの間に手元の入力から `[2]` へ入れるビット。手元の入力（`Game::local_input`）は、台本の語なら `MATCH_SEND_MASK` を
/// 当てた語、デバイスの語は poll の生の語で、Q（`pause::KEY_TO_TITLE`。どの状態からでも「タイトルへ戻る」へ行く `0x004347AD`）・
/// P/Home（`0x0800`。ゲーム自身のスクリーンショット `0x0042DBAA`）・Enter などを含みうる。ネットワークで送る語と同じマスクで切ってこれらを落とす。
pub(super) const PAUSE_KEYS: u16 = MATCH_SEND_MASK;
const _: () = assert!(
    PAUSE_KEYS & pause::KEY_TO_TITLE == 0 && PAUSE_KEYS & 0x0800 == 0 && PAUSE_KEYS & ESC != 0
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LocalPause {
    Off,
    /// Esc を入れた tick の次（ポーズが開いたかを見る）。
    Opening,
    /// ゲームのポーズが開いている。
    Open,
    /// 閉じる tick に次の `Frame` を使いに行った（記録が無ければ描画だけのフレームで、次の tick もここ）。`prev2` はその前の `[2]`。
    Closing {
        prev2: u16,
    },
    /// 閉じる tick に `Frame` を使った。次の tick に閉じたかを見る。`reopen` はその `Frame` の `[2]` に Esc の押した瞬間があった
    /// （ホストがそのフレームでポーズを開いた。閉じた直後に開き直すのが正しい）。
    Closed {
        reopen: bool,
    },
    /// 重ね描きの一時停止（描画だけのフレーム。記録を使わない）。
    Overlay,
}

/// 一時停止の数（本ログ用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PauseStats {
    /// ゲームのポーズを開いた回数と、開いている間の tick（Esc の tick を除く）。
    pub opened: u32,
    pub ticks: u64,
    /// 閉じる tick に次の `Frame` を使った回数。
    pub resumed: u32,
    /// 閉じた直後にホストのポーズが開き直した回数（正しい動き）。
    pub reopened: u32,
    /// 崩れ: Esc の tick にポーズが開かなかった（試合が最後の語で 1 フレーム進んだ）。
    pub open_slips: u32,
    /// 崩れ: 閉じる tick に閉じなかった・その前に閉じた・開き直しの食い違い。
    pub close_slips: u32,
    /// ポーズの状態を読めず、描画だけのフレームにした tick。
    pub unreadable: u32,
    /// 崩れた seg で、ゲームのポーズの代わりに重ね描きの一時停止にした回数。
    pub fallbacks: u32,
    /// ポーズの「タイトルへ戻る」で観戦を終えた回数（0 か 1）。
    pub quits: u32,
    /// 重ね描きの一時停止の回数と、その tick。
    pub overlays: u32,
    pub overlay_ticks: u64,
    /// マッチの外の Esc の押下（押した瞬間）の数。
    pub esc_outside: u32,
}

/// 左上の 1 行に出す観戦者の知らせ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchNotice {
    /// 次のキャラ選択から映る（`WatchPlay::awaiting_select`）。
    NextSelect,
    /// 重ね描きの一時停止。
    Paused,
    /// マッチの外で Esc を押している（3 秒の長押しで観戦をやめる）。
    EscHold,
}

impl WatchPlay {
    /// マッチの中の tick の一時停止の判断。`Some` はこの tick の答え（記録を使わない）、`None` は記録の経路（`match_frame`）へ進む。
    pub(super) fn pause_tick<G: Game>(&mut self, game: &mut G, esc: bool) -> Option<Action> {
        if let LocalPause::Closed { reopen } = self.pause {
            self.check_closed(game, reopen);
        }
        match self.pause {
            LocalPause::Off => esc.then(|| self.open_pause(game)),
            LocalPause::Overlay => {
                self.mark_used();
                if esc {
                    self.pause = LocalPause::Off;
                    return None;
                }
                self.stats.pause.overlay_ticks = self.stats.pause.overlay_ticks.saturating_add(1);
                Some(Action::RenderOnly)
            }
            LocalPause::Opening | LocalPause::Open | LocalPause::Closing { .. } => {
                self.game_pause(game)
            }
            LocalPause::Closed { .. } => None,
        }
    }

    /// 記録の経路の後: 閉じる tick に `Frame` を使えたら、次の tick に閉じたかを見る。
    pub(super) fn after_close(&mut self, a: Action) {
        if let (LocalPause::Closing { prev2 }, Action::Played(r)) = (self.pause, a) {
            self.pause = LocalPause::Closed {
                reopen: (r.p1 | r.p2) & ESC & !prev2 != 0,
            };
            self.stats.pause.resumed = self.stats.pause.resumed.saturating_add(1);
        }
    }

    /// Esc の tick。ゲームのポーズを開けるなら `[2]` に最後の語と Esc の押した瞬間を入れる（記録は使わない）。開けないなら重ね描きの一時停止。
    fn open_pause<G: Game>(&mut self, game: &mut G) -> Action {
        self.mark_used();
        let (p1, p2) = self.last_words;
        // GameManager が開くのは、ポーズ・終わりのメニュー・読み込みのどれでもなく `[2]+0x06 & 8` のとき（`0x0041AB4E`〜`0x0041AB7B`）。
        // 前の語に Esc が入っていると押した瞬間を作れない。
        let can = game
            .play_view()
            .is_some_and(|v| !v.loading && !v.paused && !v.end_menu)
            && (p1 | p2) & ESC == 0;
        if !can || self.pause_broken {
            if can {
                self.stats.pause.fallbacks = self.stats.pause.fallbacks.saturating_add(1);
            }
            self.pause = LocalPause::Overlay;
            let s = &mut self.stats.pause;
            s.overlays = s.overlays.saturating_add(1);
            s.overlay_ticks = s.overlay_ticks.saturating_add(1);
            return Action::RenderOnly;
        }
        game.feed(2, p1 | p2 | ESC);
        self.pause = LocalPause::Opening;
        self.stats.pause.opened = self.stats.pause.opened.saturating_add(1);
        Action::Zero
    }

    /// ゲームのポーズが開いている間の 1 tick（入力の地点で読んだ状態と数えは、前の tick の終わりの値）。
    fn game_pause<G: Game>(&mut self, game: &mut G) -> Option<Action> {
        self.mark_used();
        let view = game.play_view();
        if self.pause == LocalPause::Opening {
            match view {
                Some(v) if v.paused => self.pause = LocalPause::Open,
                Some(_) => {
                    self.stats.pause.open_slips = self.stats.pause.open_slips.saturating_add(1);
                    self.slip(LocalPause::Off);
                    return None;
                }
                None => return Some(self.unreadable()),
            }
        }
        let menu = match view {
            Some(PlayView {
                paused: true,
                pause: Some(m),
                ..
            }) => m,
            Some(PlayView { paused: false, .. }) => {
                // 閉じる tick を待たずに閉じた（試合が最後の語で進んだ）。
                self.stats.pause.close_slips = self.stats.pause.close_slips.saturating_add(1);
                self.slip(LocalPause::Off);
                return None;
            }
            _ => return Some(self.unreadable()),
        };
        self.stats.pause.ticks = self.stats.pause.ticks.saturating_add(1);
        let (p1, p2) = self.last_words;
        let w2 = match menu.state {
            pause::LEAVING => {
                // 「タイトルへ戻る」→「はい」。観戦を終え、以後この状態を Esc で再開へ戻させない（`finish` が `leaving` を立てる）。
                self.stats.pause.quits = self.stats.pause.quits.saturating_add(1);
                self.quit_by_esc();
                self.pause = LocalPause::Off;
                return Some(self.leaving_tick(game));
            }
            pause::RESUMING if menu.frames >= CLOSE_FRAMES => {
                self.pause = LocalPause::Closing { prev2: p1 | p2 };
                return None;
            }
            pause::RESUMING => p1 | p2,
            _ => game.local_input(Seat::P1) & PAUSE_KEYS,
        };
        game.feed(2, w2);
        Some(Action::Zero)
    }

    /// 閉じる tick の次: 閉じたか（`reopen` なら開き直したか）を見る。
    fn check_closed<G: Game>(&mut self, game: &mut G, reopen: bool) {
        self.pause = LocalPause::Off;
        let ok = match game.play_view() {
            Some(PlayView { paused: false, .. }) => !reopen,
            Some(PlayView {
                paused: true,
                pause: Some(m),
                ..
            }) if m.state == pause::INIT => {
                if reopen {
                    self.stats.pause.reopened = self.stats.pause.reopened.saturating_add(1);
                }
                reopen
            }
            Some(_) => {
                // 閉じなかった。閉じる tick に使った `Frame` は試合に入っていない（取り戻さない）。このポーズの扱いは続ける。
                self.pause = LocalPause::Open;
                false
            }
            None => {
                self.stats.pause.unreadable = self.stats.pause.unreadable.saturating_add(1);
                true
            }
        };
        if !ok {
            self.stats.pause.close_slips = self.stats.pause.close_slips.saturating_add(1);
            self.pause_broken = true;
        }
    }

    /// 崩れた: この seg の残りはゲームのポーズを開かない（`begin_seg` で戻す）。
    fn slip(&mut self, next: LocalPause) {
        self.pause_broken = true;
        self.pause = next;
    }

    fn unreadable(&mut self) -> Action {
        self.stats.pause.unreadable = self.stats.pause.unreadable.saturating_add(1);
        Action::RenderOnly
    }

    /// マッチの中で観戦を終えた後、場面がマッチを出るまでの tick（`leaving`）。`[2]` だけをこちらが作り、本物のキーボードで試合を進めない:
    /// ポーズが開いていれば Q の押した瞬間で「タイトルへ戻る」（状態 8。20 tick 後に次の要求 1）へ送り、状態 8 の間は何も押さない（Esc は
    /// 状態 8 を再開へ戻す `0x0043474B`）。開いていなければ Esc の押した瞬間で開く。終わりのメニュー・読めないときは、手元の入力から Esc と Q を
    /// 落として入れる（メニューは観戦者が選ぶ）。押した瞬間は 1 tick おきに作る。
    pub(super) fn leaving_tick<G: Game>(&mut self, game: &mut G) -> Action {
        let w2 = match game.play_view() {
            Some(PlayView {
                paused: true,
                pause: Some(m),
                ..
            }) if m.state == pause::LEAVING || m.state == pause::INIT => 0,
            Some(PlayView { paused: true, .. }) => self.leave_press(pause::KEY_TO_TITLE),
            Some(PlayView {
                loading: false,
                end_menu: false,
                ..
            }) => self.leave_press(ESC),
            _ => game.local_input(Seat::P1) & PAUSE_KEYS & !ESC,
        };
        game.feed(2, w2);
        Action::Zero
    }

    fn leave_press(&mut self, key: u16) -> u16 {
        self.leave_pulse = !self.leave_pulse;
        if self.leave_pulse { key } else { 0 }
    }
}
