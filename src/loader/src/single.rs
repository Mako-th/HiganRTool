//! 同じ th09.exe（同じパス）が動いている間は、もう 1 つ起動しない。
//!
//! 起動する花映塚は名前つきの二重起動の印を持たない（`launch::ISOLATED_INSTANCE`）ので、何も見ないと Enter のキーリピートなどで
//! 何個でも起動する。起動の前に、プロセスの一覧から同じパスの th09.exe を探す。別のフォルダのコピーは断らない。
//!
//! 2 つのローダーがほぼ同時に走ると、互いの th09.exe がまだ一覧に無い瞬間がある。そこで確認から起動の終わりまでを、
//! ゲームのパスから決めた名前のミューテックスで 1 つずつにする。取れなかった（別のローダーが起動の途中の）ときも断る。
//! 一覧もミューテックスも取れなければ、断らずに起動する（ここで起動できなくなるより、多重起動の方がまし）。
//! unsafe は無い（Win32 は `Host` トレイト越し）。

use std::path::Path;

use higanr_core::game_running::{lock_name, same_name_pids, same_path};
use higanr_core::lang::Tx;

pub type ErrorCode = u32;

/// ミューテックスの取得の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    Held(usize),
    /// 別のローダーが持っている。
    Busy,
    Failed(ErrorCode),
}

pub trait Host {
    /// 待たずに取る。前の持ち主が持ったまま終わっていた場合も取れたこととする。
    fn lock(&mut self, name: &str) -> Lock;
    fn unlock(&mut self, token: usize);
    /// 全プロセスの (pid, 実行ファイル名)。
    fn processes(&mut self) -> Result<Vec<(u32, String)>, ErrorCode>;
    /// 権限などで開けなければ None。
    fn image_path(&mut self, pid: u32) -> Option<String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// 同じパスの th09.exe が動いている。
    Running(u32),
    /// 別のローダーが同じパスの th09.exe を起動している途中。
    Starting,
}

/// 取っているミューテックス（取れなかったときは None）。`leave` で放す。
#[derive(Debug, PartialEq, Eq)]
pub struct Held(Option<usize>);

pub const ALREADY_RUNNING: Tx = Tx {
    ja: "花映塚はすでに起動しています。",
    en: "Touhou 9 is already running.",
};

/// 確認する起動か。`--second` と台本の起動（`--script`）は開発用で、同じ花映塚を続けて起こすので確認しない。
pub fn is_checked(second: bool, script: Option<&Path>) -> bool {
    !second && script.is_none()
}

/// `game` は絶対パス。`log` は段階ログへ書く関数。
pub fn enter<H: Host>(
    h: &mut H,
    game: &Path,
    own_pid: u32,
    log: &dyn Fn(&str),
) -> Result<Held, Refusal> {
    // 短い名前（8.3）や大文字・小文字を、実際のファイルの綴りにそろえる。ファイルが無ければ渡されたまま。
    let real = std::fs::canonicalize(game).unwrap_or_else(|_| game.to_path_buf());
    let target = real.to_string_lossy();

    let held = match h.lock(&lock_name(&target)) {
        Lock::Held(t) => Some(t),
        Lock::Busy => {
            log("多重起動の確認: 別のローダーが同じゲームを起動している途中のため、起動しない");
            return Err(Refusal::Starting);
        }
        Lock::Failed(e) => {
            log(&format!(
                "多重起動の確認: ミューテックスを取れなかった（GetLastError {e}）。確認だけ行う"
            ));
            None
        }
    };

    let running = match h.processes() {
        Ok(list) => same_name_pids(&list, &target, own_pid)
            .into_iter()
            .find(|&pid| {
                h.image_path(pid)
                    .is_some_and(|image| same_path(&image, &target))
            }),
        Err(e) => {
            log(&format!(
                "多重起動の確認: プロセスの一覧を取れなかった（GetLastError {e}）。確認せずに起動する"
            ));
            None
        }
    };
    if let Some(pid) = running {
        if let Some(t) = held {
            h.unlock(t);
        }
        log(&format!(
            "多重起動の確認: 同じパスの th09.exe が動いている（pid {pid}）ので、起動しない"
        ));
        return Err(Refusal::Running(pid));
    }
    log("多重起動の確認: 同じパスの th09.exe は動いていない");
    Ok(Held(held))
}

/// 起動が終わったら（成功も失敗も）呼ぶ。起動した th09.exe はもう一覧に載っているので、次のローダーには見える。
pub fn leave<H: Host>(h: &mut H, held: Held) {
    if let Some(t) = held.0 {
        h.unlock(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const GAME: &str = r"C:\fake\a\th09.exe";
    const OTHER: &str = r"C:\fake\b\th09.exe";

    #[derive(Default)]
    struct Fake {
        procs: Vec<(u32, String)>,
        images: HashMap<u32, String>,
        list_error: Option<ErrorCode>,
        lock: Option<Lock>,
        locked: Vec<String>,
        unlocked: Vec<usize>,
    }

    impl Fake {
        fn with(mut self, pid: u32, name: &str, image: Option<&str>) -> Self {
            self.procs.push((pid, name.to_string()));
            if let Some(i) = image {
                self.images.insert(pid, i.to_string());
            }
            self
        }
    }

    impl Host for Fake {
        fn lock(&mut self, name: &str) -> Lock {
            self.locked.push(name.to_string());
            self.lock.unwrap_or(Lock::Held(7))
        }
        fn unlock(&mut self, token: usize) {
            self.unlocked.push(token);
        }
        fn processes(&mut self) -> Result<Vec<(u32, String)>, ErrorCode> {
            match self.list_error {
                Some(e) => Err(e),
                None => Ok(self.procs.clone()),
            }
        }
        fn image_path(&mut self, pid: u32) -> Option<String> {
            self.images.get(&pid).cloned()
        }
    }

    fn enter_logged(h: &mut Fake, game: &str) -> (Result<Held, Refusal>, Vec<String>) {
        let lines = RefCell::new(Vec::new());
        let r = enter(h, Path::new(game), 1, &|s| {
            lines.borrow_mut().push(s.to_string())
        });
        (r, lines.into_inner())
    }

    // 何が壊れたら落ちるか: 同じパスの比べが無くなる・名前だけで断る・自分を数える、のどれでも。
    #[test]
    fn the_same_path_is_refused_and_a_copy_in_another_folder_is_not() {
        let mut h = Fake::default().with(50, "th09.exe", Some(GAME));
        let (r, _) = enter_logged(&mut h, GAME);
        assert_eq!(r, Err(Refusal::Running(50)));
        assert_eq!(h.unlocked, vec![7], "断るときはミューテックスを放す");

        let mut h = Fake::default().with(50, "th09.exe", Some(OTHER));
        let (r, _) = enter_logged(&mut h, GAME);
        assert_eq!(r, Ok(Held(Some(7))));
        assert!(h.unlocked.is_empty());
    }

    #[test]
    fn the_path_compare_ignores_case_and_the_extended_prefix() {
        let mut h = Fake::default().with(50, "TH09.EXE", Some(r"\\?\c:\FAKE\A\Th09.exe"));
        assert_eq!(enter_logged(&mut h, GAME).0, Err(Refusal::Running(50)));
    }

    #[test]
    fn a_process_we_cannot_open_is_skipped_and_a_later_one_still_counts() {
        let mut h = Fake::default()
            .with(50, "th09.exe", None)
            .with(51, "th09.exe", Some(GAME));
        assert_eq!(enter_logged(&mut h, GAME).0, Err(Refusal::Running(51)));

        let mut h = Fake::default().with(50, "th09.exe", None);
        assert!(enter_logged(&mut h, GAME).0.is_ok());
    }

    #[test]
    fn our_own_pid_and_other_programs_do_not_count() {
        let mut h =
            Fake::default()
                .with(1, "th09.exe", Some(GAME))
                .with(2, "notepad.exe", Some(GAME));
        assert!(enter_logged(&mut h, GAME).0.is_ok());
    }

    #[test]
    fn a_busy_lock_refuses_before_looking_at_processes() {
        let mut h = Fake {
            lock: Some(Lock::Busy),
            list_error: Some(5),
            ..Fake::default()
        };
        let (r, lines) = enter_logged(&mut h, GAME);
        assert_eq!(r, Err(Refusal::Starting));
        assert!(h.unlocked.is_empty());
        assert!(
            lines.iter().any(|l| l.contains("起動している途中")),
            "{lines:?}"
        );
    }

    #[test]
    fn the_lock_name_follows_the_game_path() {
        let mut a = Fake::default();
        let mut b = Fake::default();
        let mut a2 = Fake::default();
        let _ = enter_logged(&mut a, GAME);
        let _ = enter_logged(&mut b, OTHER);
        let _ = enter_logged(&mut a2, &GAME.to_uppercase());
        assert_ne!(a.locked, b.locked, "別のフォルダは別のミューテックス");
        assert_eq!(a.locked, a2.locked, "大文字・小文字は同じミューテックス");
    }

    #[test]
    fn an_unreadable_list_or_lock_does_not_stop_the_launch() {
        let mut h = Fake {
            list_error: Some(5),
            ..Fake::default()
        };
        let (r, lines) = enter_logged(&mut h, GAME);
        assert_eq!(r, Ok(Held(Some(7))));
        assert!(
            lines.iter().any(|l| l.contains("一覧を取れなかった")),
            "{lines:?}"
        );

        let mut h = Fake {
            lock: Some(Lock::Failed(5)),
            ..Fake::default()
        };
        let (r, lines) = enter_logged(&mut h, GAME);
        assert_eq!(r, Ok(Held(None)));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("ミューテックスを取れなかった")),
            "{lines:?}"
        );
        // 取れていないものは放さない。
        leave(&mut h, r.unwrap());
        assert!(h.unlocked.is_empty());
    }

    // 何が壊れたら落ちるか: 台本の回・`--second` まで断る（検査の回が止まる）・普通の起動を確認しない（多重起動が戻る）。
    #[test]
    fn only_a_plain_launch_is_checked() {
        assert!(is_checked(false, None));
        assert!(!is_checked(true, None));
        assert!(!is_checked(false, Some(Path::new(r"C:\s\a.txt"))));
        assert!(!is_checked(true, Some(Path::new(r"C:\s\a.txt"))));
    }

    #[test]
    fn leave_releases_the_lock_it_holds() {
        let mut h = Fake::default();
        let held = enter_logged(&mut h, GAME).0.unwrap();
        leave(&mut h, held);
        assert_eq!(h.unlocked, vec![7]);
    }

    // 何が壊れたら落ちるか: 本物の一覧に自分が載らない・自分のパスが読めない・パスの比べが本物の形（`C:\...`）とずれる。
    #[test]
    fn the_real_list_finds_this_test_process_by_path() {
        let mut real = crate::real::RealSingle;
        let me = std::process::id();
        let exe = std::env::current_exe().unwrap();
        let list = real.processes().expect("一覧が取れるはず");
        let name = exe.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(
            list.iter()
                .any(|(pid, n)| *pid == me && n.to_lowercase() == name),
            "自分が一覧に無い"
        );
        let image = real.image_path(me).expect("自分のパスは読めるはず");
        assert!(same_path(&image, &exe.to_string_lossy()), "{image}");
    }

    // 何が壊れたら落ちるか: 取れなかった側が Busy にならない（同時に起動した 2 つのローダーが両方通る）・放しても取れない。
    #[test]
    fn the_real_mutex_lets_only_one_thread_in_until_it_is_released() {
        let name = lock_name(&format!(r"C:\single_test\{}\th09.exe", std::process::id()));
        let mut real = crate::real::RealSingle;
        let Lock::Held(token) = real.lock(&name) else {
            panic!("最初は取れるはず");
        };
        let other = |n: String| {
            std::thread::spawn(move || {
                let mut r = crate::real::RealSingle;
                let got = r.lock(&n);
                if let Lock::Held(t) = got {
                    r.unlock(t);
                }
                got
            })
            .join()
            .unwrap()
        };
        assert_eq!(other(name.clone()), Lock::Busy);
        real.unlock(token);
        assert!(matches!(other(name), Lock::Held(_)));
    }

    #[test]
    fn the_refusal_text_is_in_both_languages() {
        assert!(ALREADY_RUNNING.ja.contains("すでに起動しています。"));
        assert!(ALREADY_RUNNING.en.contains("already running"));
        assert!(!higanr_core::lang::has_japanese(ALREADY_RUNNING.en));
    }
}
