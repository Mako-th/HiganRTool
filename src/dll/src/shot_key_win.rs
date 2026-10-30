//! 自分のPCにだけゲーム自身のスクリーンショットを撮らせる仕組みのWin32の層（unsafeはこの中だけ）。押した瞬間の取り方と持ち越しの規則は`shot_key.rs`。
//!
//! - 毎tick（`script_win::tick_inner`の終わり。ツールが入力を占有したtickかどうかを渡される）に次を行う。
//!   ゲームのウィンドウが前面で文字の入力中でないときだけP・Homeを`GetAsyncKeyState`で読んで押した瞬間を覚える（読むだけ。OSへの入力の注入も前面の操作もしない）。
//!   台本の付いた実行では読まない。
//! - Presentのラッパ`0x0042DB50`を呼ぶ間（`pacing_win::present_entry`が`shot_present_begin` / `shot_present_end`で挟む）に次を行う。
//!   覚えた押下があれば`SHOT_WORD`（合成のInputManagerの押した瞬間のワード）の`SHOT_BIT`を立て、戻ったらそのビットだけを落とす。
//!   ゲームがラッパの中の`0x0042DBAA`でビットを見て、自分の`snapshot\`にBMPを書く。
//!   DLLはファイルを書かず、ゲームのコードも書き換えない（データの1ワードを一時的に書くだけ）。
//!
//! 遅延の増分は、毎tickに旗の直読み1回・前面の問い合わせ3回・`GetAsyncKeyState`最大2回（前面のときだけ）。
//! 押したフレームだけ、ゲーム自身のBMPの書き込み（約0.9 MB。ミリ秒は測っていない）がPresentのラッパの中で走る。
//! ゲームのスレッドで走るが、確保しない・待たない。

use core::sync::atomic::{AtomicU32, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::digit_keys::may_read;
use crate::pacing_win::PRESENT_ORIGINAL;
use crate::shot_key::{self, ShotKeys};

/// 仮想キーP（文字キーの仮想キーコードは大文字のASCII）とHome（`VK_HOME`）。ゲームは両方を同じ`0x0800`に割り当てている。
const VK_P: u16 = 0x50;
const VK_HOME: u16 = 0x24;

/// `ShotKeys`の2ビットの形。書き手も読み手もゲームのスレッドだけ（パニックの受けも同じスレッド）。
static STATE: AtomicU32 = AtomicU32::new(ShotKeys::new().to_bits());
/// Presentのラッパの間に`SHOT_BIT`を立てている（戻ったら落とす）。
static RAISED: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
static TEST_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// 検査の偽の読み: `-1`は読んではいけない間、`0` / `1`はP・Homeが押されていない / いる。
#[cfg(test)]
static TEST_DOWN: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);
#[cfg(test)]
static TEST_WORD: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);
#[cfg(test)]
static TEST_ADDR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 毎tickの読みとPresentの挟みを動かしてよいかどうか。Presentのラッパの差し替え口を設置できた実行だけ動かす（できなかった実行は撮れない・何も書かない）。
/// 検査では他の検査が`PRESENT_ORIGINAL`を触るのでこの旗を立てた検査だけが動かす。
pub fn armed() -> bool {
    #[cfg(test)]
    if !TEST_ON.load(Ordering::SeqCst) {
        return false;
    }
    PRESENT_ORIGINAL.load(Ordering::Acquire) != 0
}

/// 状態を最初に戻す（入口でパニックを受けたとき）。
pub fn clear() {
    STATE.store(ShotKeys::new().to_bits(), Ordering::Relaxed);
}

/// P・Homeの今の押下を読んでよい間だけ読む。`active`はゲームの前面の旗`APP_ACTIVE`（読めなければ`None`）。
/// ほかのアプリが前面のときと、ルーム名などの文字の入力中は読まない（打ったPでスクリーンショットを撮らない）。
/// ゲートの式は数字キーの読みと同じ`may_read`。
fn read_down_with(
    active: impl FnOnce() -> Option<u32>,
    foreground: impl FnOnce() -> bool,
    text_input_open: impl FnOnce() -> bool,
    mut down: impl FnMut(u16) -> bool,
) -> Option<bool> {
    let on = active()? != 0;
    if !may_read(on, foreground) || text_input_open() {
        return None;
    }
    Some(down(VK_P) || down(VK_HOME))
}

#[cfg(not(test))]
fn read_down() -> Option<bool> {
    read_down_with(
        // 旗はth09.exeの .dataにあり、起動から終わりまで読める（直に読む）。
        || crate::rollback_play_win::read_fast(crate::th09_addr::APP_ACTIVE),
        crate::digit_keys_win::foreground_is_game,
        crate::text_input_win::gate_open,
        crate::digit_keys_win::key_down,
    )
}

#[cfg(test)]
fn read_down() -> Option<bool> {
    match TEST_DOWN.load(Ordering::SeqCst) {
        -1 => None,
        v => Some(v != 0),
    }
}

/// 1 tickに1回呼ぶ。
/// `held`はこのtickでツールが入力を占有したか（入力の入口の戻り値が0でない）。
/// `scripted`は台本の付いた実行か（台本の実行では本物のキーを読まない）。
/// パニックは受けて状態を最初に戻す。
pub fn on_tick(held: bool, scripted: bool) {
    on_tick_with(held, scripted, read_down);
}

fn on_tick_with(held: bool, scripted: bool, read: impl FnOnce() -> Option<bool>) {
    let r = catch_unwind(AssertUnwindSafe(|| {
        let mut s = ShotKeys::from_bits(STATE.load(Ordering::Relaxed));
        let down = if scripted { None } else { read() };
        s.tick(down, held && !scripted);
        STATE.store(s.to_bits(), Ordering::Relaxed);
    }));
    if r.is_err() {
        clear();
    }
}

/// 検査だけ。`script_win`の検査が入力の入口の戻り値と台本の有無から押下を取れているかを見るための入口。
/// `on`で毎tickの読みを動かす（状態は最初に戻す）。
#[cfg(test)]
pub(crate) fn test_enable(on: bool) {
    TEST_ON.store(on, Ordering::SeqCst);
    TEST_DOWN.store(-1, Ordering::SeqCst);
    STATE.store(ShotKeys::new().to_bits(), Ordering::SeqCst);
}

/// 検査だけ: 偽のキーの読み（`None`は読んではいけない間）。
#[cfg(test)]
pub(crate) fn test_set_down(down: Option<bool>) {
    TEST_DOWN.store(down.map_or(-1, i32::from), Ordering::SeqCst);
}

/// 検査だけ: 持ち越した押下があるかどうか。
#[cfg(test)]
pub(crate) fn test_pending() -> bool {
    ShotKeys::from_bits(STATE.load(Ordering::SeqCst)).pending()
}

/// ゲームのワードの場所。製品では`SHOT_WORD`固定。検査では検査用の1ワードを指す（未設定なら`None`で何もしない）。
#[cfg(not(test))]
fn word_addr() -> Option<usize> {
    Some(crate::th09_addr::SHOT_WORD as usize)
}

#[cfg(test)]
fn word_addr() -> Option<usize> {
    Some(TEST_ADDR.load(Ordering::SeqCst)).filter(|&a| a != 0)
}

fn read_word(addr: usize) -> u16 {
    // SAFETY: 製品の`addr`は`SHOT_WORD`（th09.exeの .dataの中。起動から終わりまでコミット済みで読み書きできる頁。偶数でu16の整列）。ここへ来るのは、
    // Presentのラッパの差し替え口を設置できた実行で、ゲームのスレッド（書き手のfeed / pollと同じスレッド）が持ち越しの押下を持っているときだけ。検査では検査用の1語。
    unsafe { (addr as *const u16).read_volatile() }
}

fn write_word(addr: usize, v: u16) {
    // SAFETY: `read_word`と同じアドレス。書くのは2バイトだけで、ラッパの間の一時的な書き込み（呼び手が戻る前に`lower`で戻す）。
    unsafe { (addr as *mut u16).write_volatile(v) }
}

/// Presentのラッパを呼ぶ直前（`present_entry`が`pace_present_begin`の後に呼ぶ）。持ち越した押下があれば消してワードに`SHOT_BIT`を立てる。
/// 押下が無いframeはアトミック変数を1回読んで戻る。
pub extern "C" fn shot_present_begin() {
    let _ = catch_unwind(begin);
}

fn begin() {
    let mut s = ShotKeys::from_bits(STATE.load(Ordering::Relaxed));
    if !s.pending() {
        return;
    }
    let Some(addr) = word_addr() else {
        return;
    };
    s.take();
    STATE.store(s.to_bits(), Ordering::Relaxed);
    let Some(raised) = shot_key::raise(read_word(addr)) else {
        return;
    };
    // 先にフラグを立ててから書く（書いた後に戻せなくなる順を作らない）。
    RAISED.store(1, Ordering::Relaxed);
    write_word(addr, raised);
}

/// Presentのラッパから戻った直後（`present_entry`が`pace_present_end`の前に呼ぶ）。立てたならそのビットだけを落とす。
pub extern "C" fn shot_present_end() {
    let _ = catch_unwind(end);
}

fn end() {
    if RAISED.load(Ordering::Relaxed) == 0 {
        return;
    }
    RAISED.store(0, Ordering::Relaxed);
    let Some(addr) = word_addr() else {
        return;
    };
    write_word(addr, shot_key::lower(read_word(addr)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pacing::PACE_RING;
    use crate::pacing::tests::lock;
    use crate::pacing_win::{present_entry, set_after_present};
    use crate::stub::testkit::call_with_regs;
    use std::cell::Cell;

    static SEEN_WORD: AtomicU32 = AtomicU32::new(0);
    static WRAPPER_OR: AtomicU32 = AtomicU32::new(0);
    static WRAPPER_CALLS: AtomicU32 = AtomicU32::new(0);

    /// ゲームのラッパの代わり。呼ばれた間のワードを覚えて`WRAPPER_OR`のビットをワードに足す（立てている間に別のビットが変わる場合）。
    extern "C" fn fake_present() {
        WRAPPER_CALLS.fetch_add(1, Ordering::SeqCst);
        SEEN_WORD.store(
            u32::from(TEST_WORD.load(Ordering::SeqCst)),
            Ordering::SeqCst,
        );
        TEST_WORD.fetch_or(WRAPPER_OR.load(Ordering::SeqCst) as u16, Ordering::SeqCst);
    }

    /// 検査の用意（呼び手が`lock()`を持つ）。ワードを`word`に、押下の持ち越しを`pending`にしてPresentの差し替え口を偽のラッパへ向ける。
    fn setup(word: u16, pending: bool) {
        PACE_RING.reset();
        set_after_present(None);
        PRESENT_ORIGINAL.store(fake_present as *const () as usize as u32, Ordering::SeqCst);
        TEST_ADDR.store(TEST_WORD.as_ptr() as usize, Ordering::SeqCst);
        TEST_WORD.store(word, Ordering::SeqCst);
        WRAPPER_OR.store(0, Ordering::SeqCst);
        RAISED.store(0, Ordering::SeqCst);
        let s = if pending {
            let mut s = ShotKeys::new();
            s.tick(Some(false), true);
            s.tick(Some(true), true);
            s
        } else {
            ShotKeys::new()
        };
        assert_eq!(s.pending(), pending);
        STATE.store(s.to_bits(), Ordering::SeqCst);
    }

    /// Presentの入口を1回通す。戻り値は（ラッパが見たワード, 戻った後のワード）。
    fn present() -> (u16, u16) {
        call_with_regs(present_entry as *const () as usize as u32);
        (
            SEEN_WORD.load(Ordering::SeqCst) as u16,
            TEST_WORD.load(Ordering::SeqCst),
        )
    }

    fn teardown() {
        PACE_RING.reset();
        STATE.store(ShotKeys::new().to_bits(), Ordering::SeqCst);
        TEST_ADDR.store(0, Ordering::SeqCst);
    }

    #[test]
    fn the_bit_is_up_only_while_the_present_wrapper_runs_and_other_bits_are_untouched() {
        let _g = lock();
        setup(0x1234, true);
        let (seen, after) = present();
        assert_eq!(seen, 0x1A34, "ラッパの間に 0x0800 が立っていない");
        assert_eq!(after, 0x1234, "戻った後にほかのビットまで元の値でない");
        assert_eq!(RAISED.load(Ordering::SeqCst), 0);
        // すべてのビットが立っていても落とすのは0x0800だけ。
        setup(0x77FF, true);
        assert_eq!(present(), (0x7FFF, 0x77FF));
        teardown();
    }

    #[test]
    fn nothing_is_written_when_no_press_is_pending() {
        let _g = lock();
        setup(0x1234, false);
        let before = TEST_WORD.load(Ordering::SeqCst);
        assert_eq!(present(), (before, before), "押下が無いのに立てた");
        teardown();
    }

    #[test]
    fn one_press_makes_one_shot_not_one_per_frame() {
        let _g = lock();
        setup(0x0001, true);
        assert_eq!(present().0 & 0x0800, 0x0800, "最初の Present で立たない");
        for n in 0..3 {
            assert_eq!(
                present().0 & 0x0800,
                0,
                "押下 1 回で {} 枚目が立った",
                n + 2
            );
        }
        teardown();
    }

    #[test]
    fn only_the_bit_we_raised_is_dropped_even_if_the_game_changes_other_bits_meanwhile() {
        let _g = lock();
        setup(0x1234, true);
        WRAPPER_OR.store(0x0001, Ordering::SeqCst);
        let (seen, after) = present();
        assert_eq!(seen, 0x1A34);
        assert_eq!(
            after, 0x1235,
            "ラッパの間に変わったビットまで巻き戻した（保存値を書き戻した）か、落とし損ねた"
        );
        teardown();
    }

    #[test]
    fn a_word_that_already_has_the_bit_is_left_as_it_was() {
        let _g = lock();
        setup(0x1A34, true);
        let (seen, after) = present();
        assert_eq!(
            (seen, after),
            (0x1A34, 0x1A34),
            "ゲーム自身が立てたビットを落とした"
        );
        assert_eq!(RAISED.load(Ordering::SeqCst), 0);
        teardown();
    }

    #[test]
    fn the_wrapper_is_called_exactly_once_and_the_press_survives_skipped_frames() {
        let _g = lock();
        setup(0x0002, false);
        // 押したtickの後にPresentが来ないtickが3回続いても押下は残る。来たPresentで1回だけ立つ。
        on_tick_with(true, false, || Some(false));
        on_tick_with(true, false, || Some(true));
        for _ in 0..3 {
            on_tick_with(true, false, || Some(true));
        }
        let c0 = WRAPPER_CALLS.load(Ordering::SeqCst);
        let (seen, after) = present();
        assert_eq!((seen, after), (0x0802, 0x0002));
        assert_eq!(
            WRAPPER_CALLS.load(Ordering::SeqCst),
            c0 + 1,
            "ラッパを 1 回"
        );
        assert_eq!(present().0, 0x0002);
        teardown();
    }

    #[test]
    fn a_tool_less_tick_after_the_press_drops_it_so_the_game_does_not_shoot_twice() {
        let _g = lock();
        setup(0x0002, false);
        on_tick_with(true, false, || Some(false));
        on_tick_with(true, false, || Some(true));
        on_tick_with(false, false, || Some(true));
        assert_eq!(
            present(),
            (0x0002, 0x0002),
            "握っていない tick の後に撮った"
        );
        // ツールが入力を占有していない間に押したキーは、占有するtickになっても押下でない。
        on_tick_with(false, false, || Some(false));
        on_tick_with(false, false, || Some(true));
        on_tick_with(true, false, || Some(true));
        assert_eq!(present(), (0x0002, 0x0002), "握っていない間の押下を撮った");
        teardown();
    }

    #[test]
    fn a_scripted_run_never_reads_the_keys_or_shoots() {
        let _g = lock();
        setup(0x0002, false);
        let asked = Cell::new(0);
        let read = || {
            asked.set(asked.get() + 1);
            Some(true)
        };
        on_tick_with(true, true, || {
            asked.set(asked.get() + 1);
            Some(false)
        });
        on_tick_with(true, true, read);
        assert_eq!(asked.get(), 0, "台本の実行で本物のキーを読んだ");
        assert_eq!(present(), (0x0002, 0x0002));
        // 台本の無い実行は読む（対照）。
        on_tick_with(true, false, || {
            asked.set(asked.get() + 1);
            Some(false)
        });
        assert_eq!(asked.get(), 1);
        teardown();
    }

    #[test]
    fn a_panic_while_reading_resets_the_state_and_does_not_escape() {
        let _g = lock();
        setup(0x0002, true);
        on_tick_with(true, false, || panic!("検査: キーの読みのパニック"));
        assert_eq!(
            ShotKeys::from_bits(STATE.load(Ordering::SeqCst)),
            ShotKeys::new()
        );
        assert_eq!(present(), (0x0002, 0x0002));
        teardown();
    }

    #[test]
    fn keys_are_asked_only_when_the_game_window_is_in_front_and_not_typing() {
        let q = Cell::new(0);
        let key = |down: &'static [u16]| {
            let q = &q;
            move |vk: u16| {
                q.set(q.get() + 1);
                down.contains(&vk)
            }
        };
        let some = |v: u32| move || Some(v);
        // 旗が読めない・0・前面が別のアプリ・文字の入力中: キーを聞かない。
        assert_eq!(
            read_down_with(|| None, || true, || false, key(&[VK_P])),
            None
        );
        assert_eq!(
            read_down_with(some(0), || true, || false, key(&[VK_P])),
            None
        );
        assert_eq!(
            read_down_with(some(1), || false, || false, key(&[VK_P])),
            None
        );
        assert_eq!(
            read_down_with(some(1), || true, || true, key(&[VK_P])),
            None
        );
        assert_eq!(q.get(), 0, "読んではいけない間にキーを聞いた");
        // 旗が0なら前面の問い合わせも呼ばない（`may_read`の短絡）。
        assert_eq!(
            read_down_with(some(0), || panic!("前面を聞いた"), || false, key(&[])),
            None
        );
        // 読める間はPかHomeを読む。Pが押されていれば読む回数は1回、どちらも押されていなければ2回で済む。
        assert_eq!(
            read_down_with(some(1), || true, || false, key(&[VK_P])),
            Some(true)
        );
        assert_eq!(q.get(), 1);
        assert_eq!(
            read_down_with(some(1), || true, || false, key(&[VK_HOME])),
            Some(true)
        );
        assert_eq!(q.get(), 3);
        assert_eq!(
            read_down_with(some(1), || true, || false, key(&[0x51])),
            Some(false)
        );
        assert_eq!(q.get(), 5, "押下が無い tick のキーの問い合わせは 2 回");
        assert_eq!((VK_P, VK_HOME), (u16::from(b'P'), 0x24));
    }
}
