//! ネットワークの対戦のマッチ終了時のメニューで「リプレイを保存して終了」が決まったときゲーム自身の保存の画面（MainMenuの画面15）を飛ばしてタイトルへ戻す印の判断（pure）。
//!
//! 飛ばし方（ゲームのコードには書かない）: 画面15の副状態0の最初のフレームにゲーム自身の分岐`0x00429B5E testb $0x20,0x4a7ec5`がある。
//! この分岐はグローバル状態ワード`0x4A7EC4`のbit13（Storyのコンティニュー済みの印）を見る。
//! 印が立っていれば`0x00429B6E`で画面1（タイトル）へ移り、何も保存しない。
//! dllはこの印をネットワークの対戦で「保存して終了」が決まったtickに立てる。
//! ゲームが画面15を通ってタイトルへ着いたら落とす。
//! 落とし忘れても新規開始のたびに`0x0041B062`でbit13が落ちる。
//! そのため後のStoryの保存の画面は飛ばない。
//! 害が出るのは印が残ったまま継続の開始から画面15へ来たときだけ。落とすのはその経路を閉じるため。
//! ここにあるのはゲームのアドレスに依らない判断だけ: 立てるかどうか（`mark`）といつ落とすか（`Watch`）。
//! 分岐のアドレス・期待バイト列・印のビットはdllの`th09_addr`が持って渡す。
#![forbid(unsafe_code)]

/// MainMenuの画面の番号（`[0x4AC8C0]`の`+0x1B228`）のうちこの判断が見るもの。
/// タイトル。画面15の分岐が印を見て移る先（`0x00429B67 push $0x1` → `0x00429B6E call 0x422f39`）。
pub const SCREEN_TITLE: u32 = 1;
/// マッチの後の画面。要求6で作り直したMainMenuが最初に通る（`0x0042929C`が画面14のハンドラ）。
pub const SCREEN_AFTER_MATCH: u32 = 14;
/// リプレイの保存の画面。画面14はMatch（`0x004295B1 cmpl $0x2,0x4a7ea8`）なら`0x004295C1`でここへ移る。
/// ハンドラは`0x00429662`。副状態0の最初のフレームが`0x00429B52`〜`0x00429B65`で印を見る。
pub const SCREEN_SAVE: u32 = 15;

/// 分岐の期待バイト列の長さの上限（読むバッファの大きさ。今の行は9バイト）。
pub const SITE_MAX: usize = 16;

/// 立てる・落とすのに要るゲームのメモリ（本物はdllの`script_win`。検査では偽物）。確保もパニックもしない。
pub trait SkipMem {
    /// 分岐のバイト列を`out`の長さだけ読む。読めなければfalse。
    fn read_site(&mut self, out: &mut [u8]) -> bool;
    fn read_state(&mut self) -> Option<u32>;
    fn write_state(&mut self, v: u32) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refuse {
    /// このマッチでこのツールの自動保存が保存関数を呼んでいない。リプレイが残っていないのでいつもの保存の画面に任せる。
    NotSaved,
    SiteUnreadable,
    /// 分岐のバイト列が期待と違う（別のバージョンのexeか書き換え）。
    SiteMismatch,
    /// 照合した分岐が見るアドレス・ビットがこれから書くワードのアドレス・ビットと違う。立てても分岐は見ないので書かない。
    SiteTarget,
    StateUnreadable,
    WriteFailed,
}

impl Refuse {
    pub const ALL: [Refuse; 6] = [
        Refuse::NotSaved,
        Refuse::SiteUnreadable,
        Refuse::SiteMismatch,
        Refuse::SiteTarget,
        Refuse::StateUnreadable,
        Refuse::WriteFailed,
    ];

    pub fn text(self) -> &'static str {
        match self {
            Refuse::NotSaved => "この試合の自動保存が保存関数を呼んでいない",
            Refuse::SiteUnreadable => "分かれ道のバイト列が読めない",
            Refuse::SiteMismatch => "分かれ道のバイト列が期待と違う",
            Refuse::SiteTarget => "分かれ道が見るアドレス・ビットが書く語と違う",
            Refuse::StateUnreadable => "グローバル状態語が読めない",
            Refuse::WriteFailed => "グローバル状態語を書けなかった",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Player,
    /// 観戦者（ホストの押下を辿ったゲームが「今2・次6」にした）。
    Watcher,
}

/// `mark`の`saved`に渡す値（立ててよいかの材料）。
/// - 対戦する側: 自動保存が保存関数を呼んだときだけ真（リプレイが1本も残らないのに保存の画面を飛ばさない）。
/// - 観戦者: いつも真。観戦者にはゲームの保存の画面を出さない。
pub fn may_skip(side: Side, saved_this_seg: bool) -> bool {
    match side {
        Side::Player => saved_this_seg,
        Side::Watcher => true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marked {
    Set,
    /// 印はもう立っていた（ゲームが立てたもの）。書かず落とさない（ゲームの値を消さない）。保存の画面は飛ぶ。
    AlreadySet,
    /// 立てなかった（いつもの保存の画面）。
    Refused(Refuse),
}

/// 分岐のバイト列（`testb $imm8, abs32` = `F6 05 <アドレス 4 バイト> <即値>` で始まる形）が、アドレス`state_at`のワードのビット`bit`を見ているかどうか。
/// 期待バイト列と書くワードのアドレスは出どころが別なので、書く前にこの2つを結び付ける。
/// 埋まったアドレスはワードの中の1バイト（`state_at` 〜 `state_at + 3`）で、即値をそのバイトの位置へずらしたものが`bit`に等しいこと。
/// 1.50aではアドレス`0x4A7EC5`（ワード`0x4A7EC4`の2バイト目）・即値`0x20` → bit13（`0x2000`）。
pub fn site_reads_bit(site: &[u8], state_at: u32, bit: u32) -> bool {
    // `F6 /0 ib`のModRM `05` = 32ビットの絶対アドレス。
    let [0xF6, 0x05, a0, a1, a2, a3, imm, ..] = *site else {
        return false;
    };
    let off = u32::from_le_bytes([a0, a1, a2, a3]).wrapping_sub(state_at);
    off < 4 && u32::from(imm) << (8 * off) == bit
}

/// 「保存して終了」が決まったtickに1回: 立ててよければ印を立てる。
/// 書く前に分岐のバイト列を`site`と照合し、合わなければ何も書かない。
/// 合ってもその分岐が`state_at`のワードの`bit`を見ていなければ書かない（`site_reads_bit`）。
/// `saved`はこのマッチで自動保存が保存関数を呼んだかどうか。
/// 書くのは読んだワードにORした`bit`だけ（ゲームのスレッドで読み書きするので他の書き手は無い）。
pub fn mark(m: &mut dyn SkipMem, saved: bool, site: &[u8], state_at: u32, bit: u32) -> Marked {
    if !saved {
        return Marked::Refused(Refuse::NotSaved);
    }
    let mut buf = [0u8; SITE_MAX];
    let Some(got) = buf.get_mut(..site.len()) else {
        // 期待バイト列がバッファより長い（表の誤り）。照合できないので書かない。
        return Marked::Refused(Refuse::SiteUnreadable);
    };
    if site.is_empty() || !m.read_site(got) {
        return Marked::Refused(Refuse::SiteUnreadable);
    }
    if got != site {
        return Marked::Refused(Refuse::SiteMismatch);
    }
    if !site_reads_bit(site, state_at, bit) {
        return Marked::Refused(Refuse::SiteTarget);
    }
    let Some(word) = m.read_state() else {
        return Marked::Refused(Refuse::StateUnreadable);
    };
    if word & bit != 0 {
        return Marked::AlreadySet;
    }
    if !m.write_state(word | bit) {
        return Marked::Refused(Refuse::WriteFailed);
    }
    Marked::Set
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Watch {
    #[default]
    Idle,
    /// 立てたtickの後まだ場面2の外を見ていない。
    Leaving,
    /// 場面2の外へ出た。マッチの後の画面（14）か保存の画面（15）をまだ見ていない。
    Away,
    /// 14か15を見た。次にタイトル（1）を見たら、ゲームが印を使い終えた。
    Passed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clear {
    Title,
    /// 場面2へ入り直した（タイトルを見ないまま次のマッチへ。念のため）。
    MatchAgain,
}

impl Watch {
    /// 立てた結果から監視を始める（このツールが立てたときだけ監視する）。
    pub fn after(m: Marked) -> Watch {
        match m {
            Marked::Set => Watch::Leaving,
            Marked::AlreadySet | Marked::Refused(_) => Watch::Idle,
        }
    }

    /// 1 tick。`in_match`は今の場面がマッチ（2）かどうか。`screen`は場面がメニュー（1）のときのMainMenuの画面（読めなければ`None`）。
    /// 落とすべきなら理由を返す（呼び手が印を落とし、落とせたら`Idle`にする —— `cleared`）。
    ///
    /// タイトルを見ただけでは落とさない。
    /// 場面1へ戻った最初のtickのMainMenuのポインタは前の（解放された）MainMenuを指しうる（`th09_addr::MAIN_MENU_PTR`）。
    /// そのため画面14か15を見てからのタイトルだけを「使い終えた」と読む。
    /// 見られなかったときは次に場面2へ入ったときに落とす。
    pub fn step(self, in_match: bool, screen: Option<u32>) -> (Watch, Option<Clear>) {
        match self {
            Watch::Idle => (Watch::Idle, None),
            Watch::Leaving if in_match => (Watch::Leaving, None),
            Watch::Leaving | Watch::Away | Watch::Passed if in_match => {
                (self, Some(Clear::MatchAgain))
            }
            Watch::Leaving | Watch::Away => match screen {
                Some(SCREEN_AFTER_MATCH | SCREEN_SAVE) => (Watch::Passed, None),
                _ => (Watch::Away, None),
            },
            Watch::Passed => match screen {
                Some(SCREEN_TITLE) => (Watch::Passed, Some(Clear::Title)),
                _ => (Watch::Passed, None),
            },
        }
    }
}

/// 印を落とす。読めない・書けないときはfalse（次のtickにやり直す）。このツールが立てたビットだけを落とす。もう落ちていれば書かずにtrue。
pub fn clear(m: &mut dyn SkipMem, bit: u32) -> bool {
    let Some(word) = m.read_state() else {
        return false;
    };
    if word & bit == 0 {
        return true;
    }
    m.write_state(word & !bit)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE: [u8; 9] = [0xF6, 0x05, 0xC5, 0x7E, 0x4A, 0x00, 0x20, 0x74, 0x13];
    const BIT: u32 = 0x2000;
    /// グローバル状態ワードのアドレス（`th09_addr::GLOBAL_STATE`。`SITE`の`testb`に埋まった`0x4A7EC5`の1つ前）。
    const AT: u32 = 0x004A_7EC4;

    struct Mem {
        site: Option<Vec<u8>>,
        state: Option<u32>,
        writable: bool,
        writes: Vec<u32>,
    }

    impl Mem {
        fn good() -> Mem {
            Mem {
                site: Some(SITE.to_vec()),
                // 関係の無い上のビットを生のMatchの値に足し、書き戻しで他のビットを落とさないかを見る。
                state: Some(0x0001_0804),
                writable: true,
                writes: Vec::new(),
            }
        }
    }

    impl SkipMem for Mem {
        fn read_site(&mut self, out: &mut [u8]) -> bool {
            match &self.site {
                Some(s) if s.len() >= out.len() => {
                    out.copy_from_slice(&s[..out.len()]);
                    true
                }
                _ => false,
            }
        }
        fn read_state(&mut self) -> Option<u32> {
            self.state
        }
        fn write_state(&mut self, v: u32) -> bool {
            if !self.writable {
                return false;
            }
            self.writes.push(v);
            self.state = Some(v);
            true
        }
    }

    #[test]
    fn marks_only_the_bit_when_saved_and_the_site_matches() {
        let mut m = Mem::good();
        assert_eq!(mark(&mut m, true, &SITE, AT, BIT), Marked::Set);
        assert_eq!(m.writes, vec![0x0001_2804]);
        let mut m = Mem::good();
        m.state = Some(0x0000_2004);
        assert_eq!(mark(&mut m, true, &SITE, AT, BIT), Marked::AlreadySet);
        assert!(m.writes.is_empty());
        assert_eq!(Watch::after(Marked::AlreadySet), Watch::Idle);
        assert_eq!(Watch::after(Marked::Set), Watch::Leaving);
    }

    #[test]
    fn a_watcher_skips_the_save_screen_even_without_an_autosave_but_a_player_does_not() {
        assert!(!may_skip(Side::Player, false));
        assert!(may_skip(Side::Player, true));
        assert!(may_skip(Side::Watcher, false));
        assert!(may_skip(Side::Watcher, true));
        let mut m = Mem::good();
        assert_eq!(
            mark(&mut m, may_skip(Side::Player, false), &SITE, AT, BIT),
            Marked::Refused(Refuse::NotSaved)
        );
        assert!(m.writes.is_empty(), "呼んでいない対戦者の試合で書いた");
        let mut m = Mem::good();
        assert_eq!(
            mark(&mut m, may_skip(Side::Watcher, false), &SITE, AT, BIT),
            Marked::Set
        );
        assert_eq!(m.writes, vec![0x0001_2804], "観戦者で立てない");
        // 観戦者でも分岐の照合は外さない。
        let mut m = Mem::good();
        let mut s = SITE.to_vec();
        s[0] ^= 0x01;
        m.site = Some(s);
        assert_eq!(
            mark(&mut m, may_skip(Side::Watcher, false), &SITE, AT, BIT),
            Marked::Refused(Refuse::SiteMismatch)
        );
        assert!(m.writes.is_empty());
    }

    #[test]
    fn refuses_without_writing() {
        let mut m = Mem::good();
        assert_eq!(
            mark(&mut m, false, &SITE, AT, BIT),
            Marked::Refused(Refuse::NotSaved)
        );
        assert!(m.writes.is_empty());
        for i in 0..SITE.len() {
            let mut m = Mem::good();
            let mut s = SITE.to_vec();
            s[i] ^= 0x01;
            m.site = Some(s);
            assert_eq!(
                mark(&mut m, true, &SITE, AT, BIT),
                Marked::Refused(Refuse::SiteMismatch),
                "位置 {i}"
            );
            assert!(m.writes.is_empty(), "位置 {i} で書いた");
        }
        let mut m = Mem::good();
        m.site = None;
        assert_eq!(
            mark(&mut m, true, &SITE, AT, BIT),
            Marked::Refused(Refuse::SiteUnreadable)
        );
        let mut m = Mem::good();
        m.site = Some(SITE[..8].to_vec());
        assert_eq!(
            mark(&mut m, true, &SITE, AT, BIT),
            Marked::Refused(Refuse::SiteUnreadable)
        );
        let mut m = Mem::good();
        assert_eq!(
            mark(&mut m, true, &[], AT, BIT),
            Marked::Refused(Refuse::SiteUnreadable)
        );
        let mut m = Mem::good();
        m.state = None;
        assert_eq!(
            mark(&mut m, true, &SITE, AT, BIT),
            Marked::Refused(Refuse::StateUnreadable)
        );
        assert!(m.writes.is_empty());
        let mut m = Mem::good();
        m.writable = false;
        assert_eq!(
            mark(&mut m, true, &SITE, AT, BIT),
            Marked::Refused(Refuse::WriteFailed)
        );
        for r in Refuse::ALL {
            assert_eq!(Watch::after(Marked::Refused(r)), Watch::Idle);
        }
    }

    #[test]
    fn refuses_when_the_site_does_not_read_the_word_it_would_write() {
        for (at, bit) in [
            (AT + 1, BIT),
            (AT - 1, BIT),
            (AT + 4, BIT),
            (AT - 4, BIT),
            (0, BIT),
            (AT, 0x20),
            (AT, 0x0020_0000),
        ] {
            let mut m = Mem::good();
            assert_eq!(
                mark(&mut m, true, &SITE, at, bit),
                Marked::Refused(Refuse::SiteTarget),
                "アドレス {at:#x} ビット {bit:#x}"
            );
            assert!(
                m.writes.is_empty(),
                "アドレス {at:#x} ビット {bit:#x} で書いた"
            );
        }
        // 照合する期待バイト列が`testb $imm8, abs32`でない（表の誤り）: 合っていても書かない。
        let mut other = SITE;
        other[1] = 0x04;
        let mut m = Mem::good();
        m.site = Some(other.to_vec());
        assert_eq!(
            mark(&mut m, true, &other, AT, BIT),
            Marked::Refused(Refuse::SiteTarget)
        );
        assert!(m.writes.is_empty());
        // ワードの他のバイトを見る分岐ならそのバイトへずらした即値で合う（ずらし方の式そのものを見る）。
        let mut b3 = SITE;
        b3[2..6].copy_from_slice(&(AT + 3).to_le_bytes());
        assert!(site_reads_bit(&b3, AT, 0x2000_0000));
        assert!(!site_reads_bit(&b3, AT, BIT));
        assert!(site_reads_bit(&SITE, AT, BIT));
        assert!(!site_reads_bit(&SITE[..6], AT, BIT), "即値の無い短い列");
    }

    #[test]
    fn watch_clears_after_the_save_screen_or_on_the_next_match() {
        let mut w = Watch::after(Marked::Set);
        let path = [
            (true, None, None),
            (false, Some(SCREEN_TITLE), None), // 前のMainMenuの残りを読んだ形。
            (false, Some(SCREEN_AFTER_MATCH), None),
            (false, Some(SCREEN_AFTER_MATCH), None),
            (false, None, None),
            (false, Some(SCREEN_SAVE), None),
            (false, Some(SCREEN_TITLE), Some(Clear::Title)),
        ];
        for (i, (in_match, screen, want)) in path.into_iter().enumerate() {
            let (next, c) = w.step(in_match, screen);
            assert_eq!(c, want, "手 {i}");
            w = next;
        }
        let mut w = Watch::Leaving;
        for (s, want) in [
            (SCREEN_AFTER_MATCH, None),
            (SCREEN_TITLE, Some(Clear::Title)),
        ] {
            let (n, c) = w.step(false, Some(s));
            assert_eq!(c, want);
            w = n;
        }
        // タイトルを見ないまま場面2へ入り直した: 落とす。立てたtickの場面2では落とさない。
        let (w, c) = Watch::Leaving.step(true, None);
        assert_eq!((w, c), (Watch::Leaving, None));
        let (w, c) = w.step(false, None);
        assert_eq!((w, c), (Watch::Away, None));
        assert_eq!(w.step(true, None).1, Some(Clear::MatchAgain));
        assert_eq!(Watch::Passed.step(true, None).1, Some(Clear::MatchAgain));
        for (m, s) in [(true, None), (false, Some(SCREEN_TITLE))] {
            assert_eq!(Watch::Idle.step(m, s), (Watch::Idle, None));
        }
    }

    #[test]
    fn clear_drops_only_the_bit() {
        let mut m = Mem::good();
        m.state = Some(0x0001_2804);
        assert!(clear(&mut m, BIT));
        assert_eq!(m.writes, vec![0x0001_0804]);
        let mut m = Mem::good();
        assert!(clear(&mut m, BIT));
        assert!(m.writes.is_empty(), "落ちているのに書いた");
        let mut m = Mem::good();
        m.state = None;
        assert!(!clear(&mut m, BIT));
        let mut m = Mem::good();
        m.state = Some(0x2000);
        m.writable = false;
        assert!(!clear(&mut m, BIT));
    }
}
