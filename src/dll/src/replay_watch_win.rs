//! 再生の監視の、ゲームのスレッドの層と本ログへ頼むスレッド。判断は`replay_watch.rs`（pure）。
//! ゲームのメモリの読みは`replay_save_win::RealSaveIo`（自動保存と同じ読み方）。unsafeは無い。
//! 常に有効にする（`HiganRTool.ini`の設定に依らない。ログは1回の再生につき1行）。入力の入口が開いている実行でだけ`on_tick`が呼ばれる。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU32, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::time::Duration;

use higanr_core::replay_playcheck::PLAY_MARK;

use crate::replay_save_win::{RealSaveIo, SaveAddrs};
use crate::replay_watch::{PlayHandoff, PlayWatch};
use crate::win::{self, read_own_memory};

/// 監視が有効かどうか。0から1にするのは`arm`。1から0にするのはパニックを受けたとき。
static ON: AtomicU32 = AtomicU32::new(0);
static HANDOFF: PlayHandoff = PlayHandoff::new();
/// ゲームのスレッドの状態。触るのはゲームのスレッドの`on_tick`だけ（`try_lock`で取れなければ何もしない）。
static WATCH: Mutex<PlayWatch> = Mutex::new(PlayWatch::new());

/// 本ログへ行を頼むスレッドが受け渡しを見に行く間隔（ms）。
pub const PUSH_PERIOD_MS: u64 = 100;

/// 有効にする（読み取りのスレッド。入力の入口を開く前）。直接読むアドレスが読み取れなければ有効にしない。
pub fn arm() -> Result<(), String> {
    if ON.load(Ordering::SeqCst) != 0 {
        return Err("設置済み".to_string());
    }
    for (addr, len) in SaveAddrs::th09().direct_ranges() {
        if read_own_memory(addr, len as usize).is_none() {
            return Err(format!(
                "直接読むアドレス 0x{addr:08X}（{len} バイト）が読み取り不能"
            ));
        }
    }
    std::thread::Builder::new()
        .name("higanrtool-playwatch".to_string())
        .spawn(|| {
            let mut read = 0u32;
            let r = catch_unwind(AssertUnwindSafe(|| {
                loop {
                    std::thread::sleep(Duration::from_millis(PUSH_PERIOD_MS));
                    let lost = HANDOFF.drain(&mut read, |r| crate::main_log::push(r.line()));
                    if lost > 0 {
                        crate::main_log::push(format!(
                            "再生の見張り: {lost} 回分の再生の行を落としました（{PLAY_MARK} の行が間に合わなかった）。"
                        ));
                    }
                }
            }));
            if r.is_err() {
                ON.store(0, Ordering::SeqCst);
                win::debug_out("HiganRTool: 再生の見張りの書き出しでパニックを受けた。見張りを止める");
            }
        })
        .map_err(|e| format!("再生の見張りのスレッドの作成不能: {e}"))?;
    ON.store(1, Ordering::Release);
    Ok(())
}

/// 入力の地点（ゲームのスレッド）から毎tick。有効にしていなければアトミック変数を1つ読んで戻る。パニックは受けて外へ出さず、監視だけを止める。
pub fn on_tick() {
    if ON.load(Ordering::Acquire) == 0 {
        return;
    }
    let r = catch_unwind(AssertUnwindSafe(|| {
        let Ok(mut w) = WATCH.try_lock() else {
            return;
        };
        let mut io = RealSaveIo {
            a: SaveAddrs::th09(),
        };
        w.tick(&HANDOFF, &mut io);
    }));
    if r.is_err() {
        ON.store(0, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    /// 呼び出しの配線: 入力の入口の毎tickの経路`route`が`on_tick`を呼ぶ。監視を有効にする`arm_replay`が`arm`を呼ぶ。
    /// どちらかが外れると、監視は黙って何も書かない。
    #[test]
    fn the_watch_is_called_every_tick_and_armed_with_the_replay_hook() {
        let src = include_str!("script_win.rs");
        let body = |head: &str, tail: &str| {
            assert_eq!(src.matches(head).count(), 1, "{head} は 1 つだけ");
            let rest = &src[src.find(head).unwrap()..];
            &rest[..rest.find(tail).unwrap()]
        };
        assert!(
            body("fn route(io: &mut RealTickIo, k: u32) -> u32 {", "\n}\n")
                .contains("crate::replay_watch_win::on_tick();")
        );
        assert!(
            body(
                "fn arm_replay(&mut self, net: Option<&NetSettings>)",
                "\n    }\n"
            )
            .contains("crate::replay_watch_win::arm()")
        );
    }
}
