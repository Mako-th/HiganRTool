//! ネットの選択のキャラ選択で、dll がカーソルを書いたとき（決定の tick と、相手の席の位置）、立ち絵と席の目印を追わせる呼び出しを
//! 決める（pure）。
//!
//! 立ち絵を動かすのはゲームの移動の経路（カーソルを ±1 → 解放されていない席を飛ばす → 立ち絵の関数 2 回 → 席の目印の関数）
//! だけで、dll はカーソルを直に書くので、書いた tick にはどれも走らない。ここは「呼ぶか・どの関数を・どの引数で」だけを決め、
//! 呼ぶのは本物の側（`net_game.rs` → `script_win.rs`）。ゲームのスレッドで走るので、確保・パニックしうる添字を置かない
//! （添字は `get`。表は 16 個で、カーソルは 16 未満に絞ってから引く）。
#![forbid(unsafe_code)]

use higanr_core::menu_model::{CHARA_BY_CURSOR, SCREEN_CHARA};

use crate::net_select::{MenuView, accepting};
use crate::th09_addr::{
    CHARA_TABLE, PORTRAIT_BASE_A, PORTRAIT_BASE_B, PORTRAIT_COUNT, PORTRAIT_STRIDE,
};

/// 呼ぶゲームの関数（アドレスは `th09_addr::PORTRAIT_SITES` の同じ名前の行）。どれも thiscall（ecx = MainMenu）で、
/// 引数は 4 つ、`ret 0x10`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortraitFn {
    /// `0x00424EDD`（割り込み 8 / 7。1P は delta > 0、2P は delta < 0 のとき）。
    Slide,
    /// `0x00424FB0`（割り込み 0xF / 0xE。`Slide` の逆の向き）。`plan` は呼ばないが、表 `PORTRAIT_SITES` の行の添字（1）と
    /// 起動のときの署名の照合は残す。
    #[allow(dead_code)] // 表の添字のためだけに残す（上の注）。
    SlideBack,
    /// `0x00425233`（席の目印とアイコンの色）。
    Marker,
}

impl PortraitFn {
    pub fn index(self) -> usize {
        match self {
            PortraitFn::Slide => 0,
            PortraitFn::SlideBack => 1,
            PortraitFn::Marker => 2,
        }
    }
}

/// 関数 1 回の呼び出し。引数は左から積む順（ecx の後の 4 つ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortraitCall {
    pub f: PortraitFn,
    pub args: [u32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// 書いたカーソルが前と同じ（立ち絵は今のままでよい）。
    Unchanged,
    /// 呼ばない（画面・状態が違う・MainMenu か部品が無い・関数の署名が合わない・手元で未解放・値が範囲の外）。
    Refused,
    /// 順に呼ぶ 3 本（ecx = `m`）。
    Calls { m: u32, calls: [PortraitCall; 3] },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Followed {
    Called,
    Unchanged,
    Refused,
}

/// 手元で未解放のキャラへ書いたときも呼ぶか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locked {
    /// 呼ばない（決定の tick のカーソル合わせ）。
    Refuse,
    /// 呼ぶ（相手の席の位置。相手の手元では解放済みで、こちらの画面にも相手のキャラの立ち絵を出す）。立ち絵の 2 本 `0x00424EDD` は
    /// 16 本の部品を全部触り、解放の印を読まない。席の目印 `0x00425233` は解放済みのアイコンの数で位置を決めるので、未解放のキャラ
    /// では次の解放済みのマスに載る。どちらも逆アセンブルからの見込みで、実機の絵では確かめていない。
    Allow,
}

/// `plan` がゲートとして読むゲームの状態（どちらも落ちない読み方で、確保しない）。
pub trait PortraitEnv {
    /// キャラが手元で解放済みか（`[0x4A81CC + キャラ]`。`0x0042306E`）。書いたカーソルのキャラだけを尋ねる（ゲームは
    /// 解放されていない席を飛ばすので、動く前のカーソルは解放済み）。
    fn unlocked(&mut self, chara: u8) -> bool;
    /// MainMenu のアドレス（呼ぶ ecx）。MainMenu と部品の配列（`[M+0x11B98]`）が 0 でなく、関数の入口のバイト列を起動のとき
    /// に照合済みのときだけ `Some`。
    fn ready(&mut self) -> Option<u32>;
}

/// カーソル（0〜15）のキャラ（表 `0x4A1E2C`。`menu_model::CHARA_BY_CURSOR`）。範囲の外は `None`。
fn chara_of(cursor: u32) -> Option<u32> {
    CHARA_BY_CURSOR.get(cursor as usize).map(|&c| u32::from(c))
}

/// 席 `seat`（1P = 0 / 2P = 1）のカーソルを `new_cursor` に書いた直後に呼ぶ 3 本を決める。`v` は書く前に読んだ選択の画面。
///
/// ゲートは決めた後（カーソルが変わり、画面が合ったとき）にだけ `env` で読む。部品の基は 1P が `0x5C` / `0x3C`、2P が `0x5D` / `0x3D`。
/// 順は「立ち絵 2 回 → 席の目印」。
///
/// 立ち絵は向きに関わらずいつも `Slide`（`0x00424EDD`）で呼ぶ。ゲームの移動の経路は向きで `0x00424EDD` / `0x00424FB0` を選ぶ
/// （`0x00428A90`〜`0x00428AA5` の `jle` と `0x00428C05`〜`0x00428C23` の `jge`）が、決定の tick に `0x00424FB0` を呼ぶと立ち絵が
/// 入れ替わらず、マップの画面まで前のキャラが残った（実測: 増える向きで 2 回とも。減る向きは入れ替わった）。
/// `0x00424FB0` は中で ANM の VM を回さない。
pub fn plan(
    v: &MenuView,
    seat: usize,
    new_cursor: u32,
    locked: Locked,
    env: &mut impl PortraitEnv,
) -> Plan {
    let other = match seat {
        0 => 1,
        1 => 0,
        _ => return Plan::Refused,
    };
    let (Some(&old), Some(&other_cursor)) = (v.chara_cursor.get(seat), v.chara_cursor.get(other))
    else {
        return Plan::Refused;
    };
    let (Some(own), Some(other_ch), Some(_)) =
        (chara_of(new_cursor), chara_of(other_cursor), chara_of(old))
    else {
        return Plan::Refused;
    };
    if old == new_cursor {
        return Plan::Unchanged;
    }
    // 画面 8 で、ゲームがこの tick に押下を読む状態のときだけ（他の画面の部品にはキャラ選択の部品が無い）。
    if !accepting(v) || v.screen != u32::from(SCREEN_CHARA) {
        return Plan::Refused;
    }
    let Ok(own_byte) = u8::try_from(own) else {
        return Plan::Refused;
    };
    if locked == Locked::Refuse && !env.unlocked(own_byte) {
        return Plan::Refused;
    }
    let Some(m) = env.ready() else {
        return Plan::Refused;
    };
    let s = seat as u32;
    let slide = PortraitFn::Slide;
    let part = |f: PortraitFn, base: u32| PortraitCall {
        f,
        args: [own, base + s, PORTRAIT_COUNT, PORTRAIT_STRIDE],
    };
    Plan::Calls {
        m,
        calls: [
            part(slide, PORTRAIT_BASE_A),
            part(slide, PORTRAIT_BASE_B),
            PortraitCall {
                f: PortraitFn::Marker,
                args: [s, own, other_ch, CHARA_TABLE],
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> MenuView {
        MenuView {
            screen: 8,
            sub: 1,
            trans: 0,
            cursor: 0,
            chara_cursor: [5, 9],
            decided: [0, 0],
            select_kind: 0,
            map_list: [0xFF; 16],
        }
    }

    const M: u32 = 0x0123_4560;

    struct Fake {
        m: Option<u32>,
        unlocked: bool,
        asked_unlocked: Vec<u8>,
        asked_ready: u32,
    }

    impl Fake {
        fn open() -> Self {
            Fake {
                m: Some(M),
                unlocked: true,
                asked_unlocked: Vec::new(),
                asked_ready: 0,
            }
        }
    }

    impl PortraitEnv for Fake {
        fn unlocked(&mut self, chara: u8) -> bool {
            self.asked_unlocked.push(chara);
            self.unlocked
        }
        fn ready(&mut self) -> Option<u32> {
            self.asked_ready += 1;
            self.m
        }
    }

    fn go(v: &MenuView, seat: usize, new: u32) -> Plan {
        plan(v, seat, new, Locked::Refuse, &mut Fake::open())
    }

    fn calls(p: Plan) -> [PortraitCall; 3] {
        match p {
            Plan::Calls { m, calls } => {
                assert_eq!(m, M, "ecx は MainMenu");
                calls
            }
            other => panic!("呼ぶはずが {other:?}"),
        }
    }

    fn call(f: PortraitFn, a: [u32; 4]) -> PortraitCall {
        PortraitCall { f, args: a }
    }

    #[test]
    fn the_site_table_matches_the_call_order() {
        use crate::sites::check_table;
        use crate::th09_addr::{
            FORBIDDEN, PORTRAIT_MARKER_SITE, PORTRAIT_SITES, PORTRAIT_SLIDE_BACK_SITE,
            PORTRAIT_SLIDE_SITE, SITES,
        };
        for (f, name, addr) in [
            (PortraitFn::Slide, PORTRAIT_SLIDE_SITE, 0x0042_4EDD),
            (PortraitFn::SlideBack, PORTRAIT_SLIDE_BACK_SITE, 0x0042_4FB0),
            (PortraitFn::Marker, PORTRAIT_MARKER_SITE, 0x0042_5233),
        ] {
            let d = &PORTRAIT_SITES[f.index()];
            assert_eq!((d.name, d.addr), (name, addr), "{f:?}");
            assert!(d.expected.len() >= 26, "{name}: 署名が短い");
            assert_eq!(d.expected[0], 0x55, "{name}: 入口は push ebp");
        }
        assert_eq!(PORTRAIT_SITES.len(), 3);
        assert_ne!(
            PORTRAIT_SITES[0].expected, PORTRAIT_SITES[1].expected,
            "スライドの 2 本は署名で区別する"
        );
        assert_eq!(check_table(PORTRAIT_SITES, FORBIDDEN), Ok(()));
        let mut both = SITES.to_vec();
        both.extend_from_slice(PORTRAIT_SITES);
        assert_eq!(check_table(&both, FORBIDDEN), Ok(()), "SITES と重ならない");
    }

    #[test]
    fn player1_forward_calls_slide_then_marker() {
        let c = calls(go(&view(), 0, 6));
        assert_eq!(
            c,
            [
                call(PortraitFn::Slide, [6, 0x5C, 16, 2]),
                call(PortraitFn::Slide, [6, 0x3C, 16, 2]),
                call(PortraitFn::Marker, [0, 6, 7, CHARA_TABLE]),
            ]
        );
        let mut v = view();
        v.chara_cursor[0] = 6;
        let c = calls(go(&v, 0, 7));
        assert_eq!(c[0].args[0], 0x0E);
        assert_eq!(c[2].args[1], 0x0E);
    }

    #[test]
    fn every_direction_uses_slide_for_both_seats() {
        for seat in [0usize, 1] {
            for old in 0..16u32 {
                for new in 0..16u32 {
                    if old == new {
                        continue;
                    }
                    let mut v = view();
                    v.chara_cursor[seat] = old;
                    let c = calls(go(&v, seat, new));
                    assert_eq!(
                        [c[0].f, c[1].f, c[2].f],
                        [PortraitFn::Slide, PortraitFn::Slide, PortraitFn::Marker],
                        "席 {seat}: {old} → {new}"
                    );
                }
            }
        }
    }

    #[test]
    fn player2_is_mirrored() {
        let c = calls(go(&view(), 1, 8));
        assert_eq!(
            c,
            [
                call(PortraitFn::Slide, [15, 0x5D, 16, 2]),
                call(PortraitFn::Slide, [15, 0x3D, 16, 2]),
                call(PortraitFn::Marker, [1, 15, 5, CHARA_TABLE]),
            ]
        );
        let c = calls(go(&view(), 1, 10));
        assert_eq!(
            c[0],
            call(PortraitFn::Slide, [8, 0x5D, 16, 2]),
            "表の [10] = 8"
        );
    }

    #[test]
    fn unchanged_cursor_calls_nothing_and_reads_nothing() {
        let mut env = Fake::open();
        for seat in [0usize, 1] {
            let same = view().chara_cursor[seat];
            assert_eq!(
                plan(&view(), seat, same, Locked::Refuse, &mut env),
                Plan::Unchanged
            );
        }
        assert_eq!((env.asked_ready, env.asked_unlocked.len()), (0, 0));
    }

    #[test]
    fn every_closed_gate_refuses() {
        let v = view();
        for bad in [
            MenuView { screen: 7, ..v },
            MenuView { screen: 16, ..v },
            MenuView { sub: 0, ..v },
            MenuView { trans: 1, ..v },
            MenuView {
                select_kind: 1,
                ..v
            },
        ] {
            assert_eq!(go(&bad, 0, 6), Plan::Refused, "{bad:?}");
        }
        let mut no_m = Fake::open();
        no_m.m = None;
        assert_eq!(
            plan(&v, 0, 6, Locked::Refuse, &mut no_m),
            Plan::Refused,
            "M か署名"
        );
        let mut locked = Fake::open();
        locked.unlocked = false;
        assert_eq!(
            plan(&v, 0, 7, Locked::Refuse, &mut locked),
            Plan::Refused,
            "未解放"
        );
        assert_eq!(
            locked.asked_unlocked,
            vec![0x0E],
            "書いたカーソル 7 の表のキャラ"
        );
        assert_eq!(locked.asked_ready, 0, "未解放なら MainMenu を読まない");
        assert_eq!(go(&v, 0, 16), Plan::Refused, "カーソルが範囲の外");
        assert_eq!(go(&v, 2, 6), Plan::Refused, "席が範囲の外");
        let mut w = v;
        w.chara_cursor[0] = 16;
        assert_eq!(go(&w, 0, 6), Plan::Refused, "前のカーソルが範囲の外");
        let mut w = v;
        w.chara_cursor[1] = 99;
        assert_eq!(go(&w, 0, 6), Plan::Refused, "相手のカーソルが範囲の外");
    }

    /// 壊れたら落ちる: 相手の席の位置の経路で未解放の門を残す（相手が手元で未解放のキャラに居ると、前のキャラの立ち絵が残る）。
    /// ほかの門は残ることも見る。
    #[test]
    fn the_peer_position_path_calls_even_for_a_locally_locked_chara() {
        let v = view();
        let mut locked = Fake::open();
        locked.unlocked = false;
        let c = calls(plan(&v, 1, 7, Locked::Allow, &mut locked));
        assert_eq!(c[0], call(PortraitFn::Slide, [0x0E, 0x5D, 16, 2]));
        assert!(locked.asked_unlocked.is_empty(), "解放の印を読まない");
        for bad in [
            MenuView { screen: 16, ..v },
            MenuView { sub: 0, ..v },
            MenuView { trans: 1, ..v },
        ] {
            let mut env = Fake::open();
            env.unlocked = false;
            assert_eq!(
                plan(&bad, 1, 7, Locked::Allow, &mut env),
                Plan::Refused,
                "{bad:?}"
            );
        }
        let mut no_m = Fake::open();
        no_m.m = None;
        assert_eq!(
            plan(&v, 1, 7, Locked::Allow, &mut no_m),
            Plan::Refused,
            "M か署名"
        );
        assert_eq!(
            plan(&v, 1, 9, Locked::Allow, &mut Fake::open()),
            Plan::Unchanged
        );
    }
}
