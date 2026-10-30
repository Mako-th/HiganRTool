//! マッチの終わりのリプレイの自動保存の判断のうちゲームのアドレスに依らないもの（pure）。
//! - いつ保存を呼ぶか（`SaveGate`。場面・メニューのフラグ・読み込みの3つをdllが真偽にして渡す）
//! - 印の結果（`result_of`。勝ち数から）と保存関数に渡す名前（`clock_name`。8バイトの`HH:MM:SS`）
//! - exeのフォルダの形の検証（`exe_dir_of`）と保存関数へ渡すANSIのパス（`ansi_vault_path`）
//! - `HiganRTool.ini`の`[replay]`の読み方（`settings_from_ini`）
//!
//! ゲームのアドレス・場面の番号・フォルダの中身はここに無い（dllが読んで渡す）。
#![forbid(unsafe_code)]

use crate::ansi::Ansi;
use crate::ini::Ini;
use crate::replay::{MatchResult, Side, Timestamp};
use crate::replay_name::DEFAULT_TEMPLATE;
use crate::vault::Split;

/// 保管庫のフォルダの名前（exeのフォルダの直下）。
pub const VAULT_DIR: &str = "Replay_HiganRTool";
pub const SECTION: &str = "replay";
/// 保存関数に渡す名前の文字数。ゲームは長さを見ないstrcpyでヘッダの9バイトのフィールドへコピーする。
/// NULを含めて収まる8文字までにする（9文字以上は隣の難易度のフィールドを壊す）。
pub const NAME_LEN: usize = 8;
/// 名前を入れるバッファの大きさ（8文字 ＋ NUL ＋ パディングの0。4の倍数）。
pub const NAME_BUF: usize = 12;
/// 保存関数に渡すパスの長さの上限（NULを除くB）。保存は`CreateFileA`を通るので`MAX_PATH`（260）からNULの1を引く。
pub const ANSI_PATH_MAX: usize = 259;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveKind {
    /// マッチ終了時のメニューが出ている。
    MenuShown,
    /// 場面2を抜ける・入り直すtick（保険）。
    /// `menu_seen`がfalseならマッチは途中で終わった（一時停止から「タイトルへ」など）。
    /// そのため印の結果はabort。
    Leaving { menu_seen: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GateScene {
    pub in_match: bool,
    /// 次の要求がマッチでない（場面2を抜ける・作り直すtick）。`in_match`のときだけ意味がある。
    pub leaving: bool,
    pub menu: bool,
    /// 読み込み中（読み込みのスレッドがリプレイの物に触りうる間）。
    pub loading: bool,
}

/// いつ保存を呼ぶかを決める（ゲームのスレッドがtickごとに`tick`を呼ぶ）。
///
/// - マッチ（場面2に入ってから抜ける・作り直すまで）ごとに保存は1回だけ。
///   メニューのフラグが`settle` tick続いたら`MenuShown`を返す。`finish`まで毎tick返す（パスの準備が間に合わなければ再試行できる）。
///   場面2を抜ける・入り直すtickで未保存なら1回だけ`Leaving`を返す（描画だけのフレームで同じtickが繰り返されても2回は返さない）。
/// - 呼んだ（か呼ばないと決めた）ら`finish`。場面2のまま次の要求も2に戻ったら（「再戦」）新しいマッチとして数え直す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveGate {
    settle: u32,
    menu_ticks: u32,
    menu_seen: bool,
    done: bool,
    leaving: bool,
    /// このマッチで保存関数を呼んだ（`mark_called`）。数え直しで解除される。
    /// ネットワーク対戦の「リプレイを保存して終了」でゲームの保存の画面を飛ばしてよいかの材料（`save_skip`）。
    called: bool,
    /// マッチの数（場面2に入る・再戦で作り直すたびに1増える。数え直しでは戻らない）。マッチごとの記録の区切りに使う。
    epoch: u32,
    in_seg: bool,
}

impl SaveGate {
    /// `settle`はメニューのフラグが何tick続いたら呼ぶか（dllはadonis rev5と同じ6）。
    pub const fn new(settle: u32) -> Self {
        SaveGate {
            settle,
            menu_ticks: 0,
            menu_seen: false,
            done: false,
            leaving: false,
            called: false,
            epoch: 0,
            in_seg: false,
        }
    }

    fn fresh(&mut self) {
        let epoch = self.epoch;
        *self = SaveGate::new(self.settle);
        self.epoch = epoch;
    }

    fn enter_seg(&mut self) {
        if !self.in_seg {
            self.in_seg = true;
            self.epoch = self.epoch.wrapping_add(1);
        }
    }

    /// マッチの番号。`tick`が新しいマッチの最初のtickで1増やす。
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    pub fn tick(&mut self, s: GateScene) -> Option<SaveKind> {
        if !s.in_match {
            self.fresh();
            return None;
        }
        self.enter_seg();
        if s.leaving {
            if self.leaving {
                return None;
            }
            self.leaving = true;
            self.menu_ticks = 0;
            return (!self.done).then_some(SaveKind::Leaving {
                menu_seen: self.menu_seen,
            });
        }
        if self.leaving {
            // 抜けるtickの後に場面2のまま次の要求も2に戻った（「再戦」）。
            self.fresh();
            self.enter_seg();
        }
        if s.menu && !s.loading {
            self.menu_ticks = self.menu_ticks.saturating_add(1);
            self.menu_seen = true;
        } else {
            self.menu_ticks = 0;
        }
        (!self.done && self.menu_ticks >= self.settle).then_some(SaveKind::MenuShown)
    }

    pub fn finish(&mut self) {
        self.done = true;
    }

    pub fn done(&self) -> bool {
        self.done
    }

    pub fn mark_called(&mut self) {
        self.called = true;
    }

    /// 場面2を抜けるtickの後も次のマッチに数え直すまでは残る（抜けるtickの後半でネットワーク処理が「保存して終了」を見るため）。
    pub fn called(&self) -> bool {
        self.called
    }
}

/// 印の結果（自分から見た）。勝ち数はサイドごと。`required`は勝ちに要る数（ゲームは勝ち数を1増やした後に`required`と比べてラウンドの演出を決める）。
/// メニューを見ずに抜けたマッチはabort。
/// 両サイドの勝ち数がrequiredに届いた場合と両サイドとも届いていない場合（メニューが出ていれば起きないはず）はdrawにする。
pub fn result_of(
    kind: SaveKind,
    me: Side,
    p1_wins: u32,
    p2_wins: u32,
    required: u32,
) -> MatchResult {
    if kind == (SaveKind::Leaving { menu_seen: false }) {
        return MatchResult::Abort;
    }
    let reached = |w: u32| required > 0 && w >= required;
    let winner = match (reached(p1_wins), reached(p2_wins)) {
        (true, false) => Some(Side::P1),
        (false, true) => Some(Side::P2),
        _ => None,
    };
    match winner {
        None => MatchResult::Draw,
        Some(w) if w == me => MatchResult::Win,
        Some(_) => MatchResult::Lose,
    }
}

/// 保存関数に渡す名前`HH:MM:SS`（8文字）の後ろにNULとパディングの0。範囲外の値（時計の誤り）でも2桁ずつに切るので8文字を越えない。
pub fn clock_name(t: &Timestamp) -> [u8; NAME_BUF] {
    let mut out = [0u8; NAME_BUF];
    for (i, v) in [t.hour, t.minute, t.second].into_iter().enumerate() {
        let v = v % 100;
        out[i * 3] = b'0' + v / 10;
        out[i * 3 + 1] = b'0' + v % 10;
        if i < 2 {
            out[i * 3 + 2] = b':';
        }
    }
    out
}

/// ゲームが持つexeのフォルダ（`char[0x104]`）を読めなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExeDirError {
    NoNul,
    Empty,
    /// 最後が`/`でない（ゲームは最後の区切りを`/`にして直後で切る）。
    NotSlash,
    /// 絶対パスでない（`GetModuleFileNameA`が失敗したときの`"./"`など）。作業フォルダに依るので使わない。
    Relative,
    NotAnsi,
}

/// バッファからexeのフォルダ（末尾の`/`を含む。NULは含まない）を取り出す。`ansi`で読めない文字があれば断る。
pub fn exe_dir_of<'a>(buf: &'a [u8], ansi: &dyn Ansi) -> Result<&'a [u8], ExeDirError> {
    let n = buf.iter().position(|&b| b == 0).ok_or(ExeDirError::NoNul)?;
    let dir = &buf[..n];
    if dir.is_empty() {
        return Err(ExeDirError::Empty);
    }
    if dir.last() != Some(&b'/') {
        return Err(ExeDirError::NotSlash);
    }
    let drive = dir.len() >= 3
        && dir[0].is_ascii_alphabetic()
        && dir[1] == b':'
        && (dir[2] == b'\\' || dir[2] == b'/');
    let unc = dir.starts_with(b"\\\\");
    if !(drive || unc) {
        return Err(ExeDirError::Relative);
    }
    if ansi.decode(dir).is_none() {
        return Err(ExeDirError::NotAnsi);
    }
    Ok(dir)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// フォルダ名かファイル名がパスの1要素でない（区切り・`.`・`..`・空）。
    BadPart(String),
    NotAnsi,
    TooLong(usize),
}

/// フォルダ名・ファイル名がパスの1要素かどうか（空・`.`・`..`・区切り・NULを含まない）。保管庫の外へ書かないためのガード。
pub fn is_single_part(p: &str) -> bool {
    !p.is_empty() && p != "." && p != ".." && !p.contains(['\\', '/', '\0'])
}

/// `<exe のフォルダ>/Replay_HiganRTool\<folder>\<file>`のANSIのバイト列（NULは含まない）。
/// `exe_dir`は`exe_dir_of`が返した形（末尾の`/`を含む。すでにANSI）。ゲームの保存もexeのフォルダを前置した形で開く。
pub fn ansi_vault_path(
    ansi: &dyn Ansi,
    exe_dir: &[u8],
    folder: Option<&str>,
    file: &str,
) -> Result<Vec<u8>, PathError> {
    for p in folder.into_iter().chain([file]) {
        if !is_single_part(p) {
            return Err(PathError::BadPart(p.to_string()));
        }
    }
    let mut rel = String::from(VAULT_DIR);
    if let Some(f) = folder {
        rel.push('\\');
        rel.push_str(f);
    }
    rel.push('\\');
    rel.push_str(file);
    let tail = ansi.encode(&rel).ok_or(PathError::NotAnsi)?;
    let mut out = Vec::with_capacity(exe_dir.len() + tail.len());
    out.extend_from_slice(exe_dir);
    out.extend_from_slice(&tail);
    if out.len() > ANSI_PATH_MAX {
        return Err(PathError::TooLong(out.len()));
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaySettings {
    pub split: Split,
    /// ファイル名のテンプレート（書かれたまま。解析と、不適切なときの代わりの名前は`replay_name`）。
    pub name: String,
}

/// 読み取るキー。これ以外は断る（綴りの誤りで既定の値のまま走らないように）。
const KEYS: [&str; 3] = ["enable", "layout", "name"];

/// `[replay]`を読む。節が無い・`enable`が無いか0なら`Ok(None)`（自動保存は動かない。既定）。
pub fn settings_from_ini(ini: &Ini) -> Result<Option<ReplaySettings>, String> {
    if !ini.has_section(SECTION) {
        return Ok(None);
    }
    for (k, line) in ini.keys(SECTION) {
        if !KEYS.contains(&k) {
            return Err(format!(
                "[{SECTION}] の {line} 行目のキー {k:?} の記法が不適切です（使用可能なキー: {}）",
                KEYS.join(" / ")
            ));
        }
    }
    match ini.get(SECTION, "enable") {
        Some("1") => {}
        Some("0") | None => return Ok(None),
        Some(v) => return Err(format!("enable の値 {v:?} は 0 か 1 にしてください")),
    }
    let split = match ini.get(SECTION, "layout") {
        None => Split::Peer,
        Some(v) => Split::parse(v).ok_or_else(|| {
            format!("layout の値 {v:?} は peer / date / flat / pack50 のどれかにしてください")
        })?,
    };
    let name = ini
        .get(SECTION, "name")
        .unwrap_or(DEFAULT_TEMPLATE)
        .to_string();
    Ok(Some(ReplaySettings { split, name }))
}

/// 保存の直前にヘッダのチャージ型をゲームの設定のコピー（生きた試合が使った値）に合わせた結果。
/// ヘッダは読み込みスレッドがそのPCの`th09.cfg`からコピーしたまま。
/// そのまま保存すると相手と設定が違う試合を素の花映塚で再生したときにずれる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChargeSync {
    /// 試みていない（保存の記録の初期値）。
    #[default]
    NotTried,
    Written {
        p1: u8,
        p2: u8,
    },
    /// コピーのポインタが0か読み取れない（書かない）。
    NoCopy,
    /// ヘッダのポインタが0か読み取れない（書かない）。
    NoHeader,
    /// コピーの中のチャージ型が読み取れない（書かない）。
    CopyUnreadable,
    /// ヘッダへ書き込めなかった。
    WriteFailed,
}

impl ChargeSync {
    /// 保存の記録（アトミック変数の1ワード）へ。下位8ビットが種類。次の8ビットが1P。その次の8ビットが2P。
    pub fn to_word(self) -> u32 {
        match self {
            ChargeSync::NotTried => 0,
            ChargeSync::Written { p1, p2 } => 1 | u32::from(p1) << 8 | u32::from(p2) << 16,
            ChargeSync::NoCopy => 2,
            ChargeSync::NoHeader => 3,
            ChargeSync::CopyUnreadable => 4,
            ChargeSync::WriteFailed => 5,
        }
    }

    pub fn from_word(w: u32) -> Option<ChargeSync> {
        Some(match w & 0xFF {
            0 => ChargeSync::NotTried,
            1 => ChargeSync::Written {
                p1: (w >> 8) as u8,
                p2: (w >> 16) as u8,
            },
            2 => ChargeSync::NoCopy,
            3 => ChargeSync::NoHeader,
            4 => ChargeSync::CopyUnreadable,
            5 => ChargeSync::WriteFailed,
            _ => return None,
        })
    }
}

/// 設定のコピーのチャージ型2バイト（`copy + copy_off`。1P・2Pの順）をヘッダの同じ並びの2バイト（`header + header_off`）へコピーする。
/// ポインタが0のとき・読み取れないときは1バイトも書かない。サイドは入れ替えない（1Pは1Pへ・2Pは2Pへ）。
/// `read` / `write`は例外を出さない読み書き（読み取れない・書き込めないアドレスは`None` / falseを返す）。
pub fn sync_header_charge(
    copy: Option<u32>,
    header: Option<u32>,
    copy_off: u32,
    header_off: u32,
    mut read: impl FnMut(u32) -> Option<u8>,
    mut write: impl FnMut(u32, u8) -> bool,
) -> ChargeSync {
    let Some(src) = copy
        .filter(|&p| p != 0)
        .and_then(|p| p.checked_add(copy_off))
    else {
        return ChargeSync::NoCopy;
    };
    let Some(dst) = header
        .filter(|&p| p != 0)
        .and_then(|p| p.checked_add(header_off))
    else {
        return ChargeSync::NoHeader;
    };
    let (Some(p1), Some(p2)) = (read(src), src.checked_add(1).and_then(&mut read)) else {
        return ChargeSync::CopyUnreadable;
    };
    let Some(dst2) = dst.checked_add(1) else {
        return ChargeSync::NoHeader;
    };
    if write(dst, p1) && write(dst2, p2) {
        ChargeSync::Written { p1, p2 }
    } else {
        ChargeSync::WriteFailed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::EncodingAnsi;

    fn sjis() -> EncodingAnsi {
        EncodingAnsi::shift_jis()
    }

    fn gbk() -> EncodingAnsi {
        EncodingAnsi::gbk()
    }

    // 既存の検査は日本語のWindows（CP932）の動き。
    fn exe_dir_of(buf: &[u8]) -> Result<&[u8], ExeDirError> {
        super::exe_dir_of(buf, &sjis())
    }

    fn ansi_vault_path(
        exe_dir: &[u8],
        folder: Option<&str>,
        file: &str,
    ) -> Result<Vec<u8>, PathError> {
        super::ansi_vault_path(&sjis(), exe_dir, folder, file)
    }

    fn scene(in_match: bool, leaving: bool, menu: bool) -> GateScene {
        GateScene {
            in_match,
            leaving,
            menu,
            loading: false,
        }
    }

    const PLAY: GateScene = GateScene {
        in_match: true,
        leaving: false,
        menu: false,
        loading: false,
    };
    const MENU: GateScene = GateScene {
        in_match: true,
        leaving: false,
        menu: true,
        loading: false,
    };
    const LEAVE: GateScene = GateScene {
        in_match: true,
        leaving: true,
        menu: false,
        loading: false,
    };

    #[test]
    fn menu_needs_settle_ticks_and_stops_after_finish() {
        let mut g = SaveGate::new(3);
        assert_eq!(g.tick(PLAY), None);
        assert_eq!(g.tick(MENU), None);
        assert_eq!(g.tick(MENU), None);
        assert_eq!(g.tick(PLAY), None);
        assert_eq!(g.tick(MENU), None);
        assert_eq!(g.tick(MENU), None);
        assert_eq!(g.tick(MENU), Some(SaveKind::MenuShown));
        // finishの前は毎tick返す（パスの準備が間に合わなかったtickの再試行）。
        assert_eq!(g.tick(MENU), Some(SaveKind::MenuShown));
        g.finish();
        for _ in 0..10 {
            assert_eq!(g.tick(MENU), None);
        }
        assert_eq!(g.tick(LEAVE), None);
        assert!(g.done());
    }

    #[test]
    fn leaving_tick_is_the_insurance_once() {
        let mut g = SaveGate::new(6);
        assert_eq!(g.tick(scene(false, false, false)), None);
        assert_eq!(g.tick(PLAY), None);
        assert_eq!(g.tick(LEAVE), Some(SaveKind::Leaving { menu_seen: false }));
        assert_eq!(g.tick(LEAVE), None, "同じ抜ける tick の繰り返し");
        assert_eq!(g.tick(LEAVE), None);
        // メニューを2 tick見た（settleに届かない）後で抜けたマッチは「見た」。
        let mut g = SaveGate::new(6);
        assert_eq!(g.tick(MENU), None);
        assert_eq!(g.tick(MENU), None);
        assert_eq!(g.tick(LEAVE), Some(SaveKind::Leaving { menu_seen: true }));
        let mut g = SaveGate::new(1);
        assert_eq!(g.tick(scene(false, true, true)), None);
    }

    #[test]
    fn rematch_and_exit_start_a_new_seg() {
        let mut g = SaveGate::new(2);
        g.tick(MENU);
        assert_eq!(g.tick(MENU), Some(SaveKind::MenuShown));
        g.finish();
        assert_eq!(g.tick(LEAVE), None);
        // 再戦: 場面2のまま次の要求が2に戻った。読み込み中のフラグは数えない。
        let loading_menu = GateScene {
            loading: true,
            ..MENU
        };
        assert_eq!(g.tick(loading_menu), None);
        assert_eq!(g.tick(loading_menu), None);
        assert_eq!(g.tick(loading_menu), None);
        assert!(!g.done(), "新しい seg なのに済みのまま");
        g.tick(MENU);
        assert_eq!(g.tick(MENU), Some(SaveKind::MenuShown), "2 試合目");
        g.finish();
        g.tick(scene(false, false, false));
        assert!(!g.done());
        g.tick(PLAY);
        assert_eq!(g.tick(LEAVE), Some(SaveKind::Leaving { menu_seen: false }));
    }

    #[test]
    fn epoch_counts_each_seg_once_even_across_rematch_and_exit() {
        let mut g = SaveGate::new(2);
        assert_eq!(g.epoch(), 0);
        // 場面2の外では増えない。
        for _ in 0..5 {
            g.tick(scene(false, false, false));
        }
        assert_eq!(g.epoch(), 0);
        // 入ったtickに1増える。同じマッチの間は増えない（メニュー・finish・抜けるtickも）。
        g.tick(PLAY);
        assert_eq!(g.epoch(), 1);
        g.tick(PLAY);
        g.tick(MENU);
        g.tick(MENU);
        g.finish();
        g.tick(MENU);
        g.tick(LEAVE);
        g.tick(LEAVE);
        assert_eq!(g.epoch(), 1);
        // 再戦: 抜けた後に場面2のまま戻ったtickで2。
        g.tick(PLAY);
        assert_eq!(g.epoch(), 2);
        g.tick(PLAY);
        assert_eq!(g.epoch(), 2);
        // 外へ出て入り直す。
        g.tick(scene(false, false, false));
        assert_eq!(g.epoch(), 2);
        g.tick(PLAY);
        assert_eq!(g.epoch(), 3);
        // 入ってすぐ抜けるマッチも1つに数える。
        g.tick(scene(false, false, false));
        g.tick(LEAVE);
        assert_eq!(g.epoch(), 4);
    }

    #[test]
    fn called_lasts_through_the_leaving_tick_and_resets_with_the_seg() {
        let mut g = SaveGate::new(2);
        g.tick(MENU);
        assert!(!g.called(), "呼ぶ前に立っている");
        assert_eq!(g.tick(MENU), Some(SaveKind::MenuShown));
        g.mark_called();
        g.finish();
        g.tick(MENU);
        assert!(g.called());
        assert_eq!(g.tick(LEAVE), None);
        assert!(g.called(), "抜ける tick に消えた");
        assert_eq!(g.tick(LEAVE), None);
        assert!(g.called(), "抜ける tick の繰り返しで消えた");
        g.tick(PLAY);
        assert!(!g.called(), "再戦の後に前の seg の値が残った");
        g.mark_called();
        g.tick(scene(false, false, false));
        assert!(!g.called(), "場面 2 の外へ出た後に残った");
        g.tick(PLAY);
        assert_eq!(g.tick(LEAVE), Some(SaveKind::Leaving { menu_seen: false }));
        g.mark_called();
        g.finish();
        assert!(g.called());
    }

    #[test]
    fn result_comes_from_the_wins() {
        let menu = SaveKind::MenuShown;
        assert_eq!(result_of(menu, Side::P1, 2, 1, 2), MatchResult::Win);
        assert_eq!(result_of(menu, Side::P2, 2, 1, 2), MatchResult::Lose);
        assert_eq!(result_of(menu, Side::P2, 0, 2, 2), MatchResult::Win);
        assert_eq!(
            result_of(menu, Side::P1, 1, 0, 2),
            MatchResult::Draw,
            "まだ届いていない"
        );
        assert_eq!(result_of(menu, Side::P1, 2, 2, 2), MatchResult::Draw);
        assert_eq!(
            result_of(menu, Side::P1, 3, 0, 0),
            MatchResult::Draw,
            "要る数 0"
        );
        assert_eq!(
            result_of(SaveKind::Leaving { menu_seen: false }, Side::P1, 2, 0, 2),
            MatchResult::Abort
        );
        assert_eq!(
            result_of(SaveKind::Leaving { menu_seen: true }, Side::P1, 2, 0, 2),
            MatchResult::Win
        );
    }

    fn ts(h: u8, m: u8, s: u8) -> Timestamp {
        Timestamp {
            year: 2026,
            month: 9,
            day: 28,
            hour: h,
            minute: m,
            second: s,
        }
    }

    #[test]
    fn clock_name_is_8_bytes_and_nul() {
        assert_eq!(&clock_name(&ts(9, 5, 3)), b"09:05:03\0\0\0\0");
        assert_eq!(&clock_name(&ts(23, 59, 59))[..9], b"23:59:59\0");
        for h in [0u8, 99, 100, 200, 255] {
            let n = clock_name(&ts(h, 255, 128));
            let len = n.iter().position(|&b| b == 0).unwrap();
            assert_eq!(len, NAME_LEN, "{h}: {n:?}");
            assert!(n[NAME_LEN..].iter().all(|&b| b == 0));
            assert!(
                n[..NAME_LEN]
                    .iter()
                    .all(|b| b.is_ascii_digit() || *b == b':')
            );
        }
    }

    #[test]
    fn exe_dir_shapes() {
        let mut b = [0u8; 0x104];
        assert_eq!(exe_dir_of(&b), Err(ExeDirError::Empty));
        b[..2].copy_from_slice(b"./");
        assert_eq!(exe_dir_of(&b), Err(ExeDirError::Relative));
        let good = b"C:\\Games\\th09/";
        let mut b = [0xAAu8; 0x104];
        b[..good.len()].copy_from_slice(good);
        b[good.len()] = 0;
        assert_eq!(exe_dir_of(&b), Ok(&good[..]));
        let mut b = [0u8; 0x104];
        b[..13].copy_from_slice(b"C:\\Games\\th09");
        assert_eq!(exe_dir_of(&b), Err(ExeDirError::NotSlash));
        assert_eq!(exe_dir_of(&[b'C'; 16]), Err(ExeDirError::NoNul));
        let unc = b"\\\\host\\share\\th09/\0";
        assert_eq!(exe_dir_of(unc), Ok(&unc[..unc.len() - 1]));
        let jp = b"C:\\\x93\x8C/\0";
        assert_eq!(exe_dir_of(jp), Ok(&jp[..jp.len() - 1]));
        assert_eq!(exe_dir_of(b"C:\\\x81/\0"), Err(ExeDirError::NotAnsi));
    }

    #[test]
    fn ansi_path_is_inside_the_vault() {
        let exe = b"C:\\G/";
        assert_eq!(
            ansi_vault_path(exe, Some("a vs b"), "th9_ud0001.rpy").unwrap(),
            b"C:\\G/Replay_HiganRTool\\a vs b\\th9_ud0001.rpy".to_vec()
        );
        assert_eq!(
            ansi_vault_path(exe, None, "th9_ud0002.rpy").unwrap(),
            b"C:\\G/Replay_HiganRTool\\th9_ud0002.rpy".to_vec()
        );
        assert_eq!(
            ansi_vault_path(exe, Some("東"), "x.rpy").unwrap(),
            b"C:\\G/Replay_HiganRTool\\\x93\x8C\\x.rpy".to_vec()
        );
        for bad in ["..", ".", "", "a\\b", "a/b"] {
            assert!(
                matches!(
                    ansi_vault_path(exe, Some(bad), "x.rpy"),
                    Err(PathError::BadPart(_))
                ),
                "{bad:?}"
            );
            assert!(matches!(
                ansi_vault_path(exe, None, bad),
                Err(PathError::BadPart(_))
            ));
        }
        assert_eq!(
            ansi_vault_path(exe, Some("😀"), "x.rpy"),
            Err(PathError::NotAnsi)
        );
        // 長さ: exe 5 ＋ "Replay_HiganRTool\" 18 ＋ ファイル名n。
        let fixed = exe.len() + VAULT_DIR.len() + 1;
        let fits = "f".repeat(ANSI_PATH_MAX - fixed);
        assert_eq!(
            ansi_vault_path(exe, None, &fits).unwrap().len(),
            ANSI_PATH_MAX
        );
        let over = "f".repeat(ANSI_PATH_MAX - fixed + 1);
        assert_eq!(
            ansi_vault_path(exe, None, &over),
            Err(PathError::TooLong(ANSI_PATH_MAX + 1))
        );
    }

    // 中国語のWindowsの「D:\东方\」（CP936で 东 = B6 AB、方 = B7 BD）。
    const GBK_EXE: &[u8] = b"D:\\\xB6\xAB\xB7\xBD/";

    #[test]
    fn a_chinese_exe_dir_is_read_as_chinese_and_not_as_shift_jis() {
        let mut b = [0u8; 0x104];
        b[..GBK_EXE.len()].copy_from_slice(GBK_EXE);
        assert_eq!(super::exe_dir_of(&b, &gbk()), Ok(GBK_EXE));
        let dir = &GBK_EXE[..GBK_EXE.len() - 1];
        assert_eq!(gbk().decode(dir).as_deref(), Some("D:\\东方"));
        // 決め打ちのCP932で読むと化けた名前（半角カナ）になる。
        let wrong = sjis().decode(dir).unwrap();
        assert!(wrong.starts_with("D:\\ｶｫ"), "{wrong}");
        // CP936にだけある文字（A9 96 = 〇）を含む名前はCP932では読めない。
        let only_gbk = b"D:\\\xA9\x96/\0";
        assert_eq!(
            super::exe_dir_of(only_gbk, &gbk()),
            Ok(&only_gbk[..only_gbk.len() - 1])
        );
        assert_eq!(
            super::exe_dir_of(only_gbk, &sjis()),
            Err(ExeDirError::NotAnsi)
        );
        // 読めない並びは断る。
        assert_eq!(
            super::exe_dir_of(b"C:\\\x81/\0", &gbk()),
            Err(ExeDirError::NotAnsi)
        );
    }

    #[test]
    fn the_ansi_path_for_a_chinese_exe_dir_keeps_its_bytes() {
        let g = gbk();
        let got =
            super::ansi_vault_path(&g, GBK_EXE, Some("东方 vs ab"), "th9_ud0001.rpy").unwrap();
        let mut want = GBK_EXE.to_vec();
        want.extend_from_slice(b"Replay_HiganRTool\\");
        want.extend_from_slice(&[0xB6, 0xAB, 0xB7, 0xBD]);
        want.extend_from_slice(b" vs ab\\th9_ud0001.rpy");
        assert_eq!(got, want);
        // 日本語のWindowsのCP932では同じ名前の「东」は表せないので保存しない。
        assert_eq!(
            super::ansi_vault_path(&sjis(), GBK_EXE, Some("东"), "x.rpy"),
            Err(PathError::NotAnsi)
        );
        assert_eq!(
            super::ansi_vault_path(&g, GBK_EXE, Some("ｱ"), "x.rpy"),
            Err(PathError::NotAnsi)
        );
    }

    #[test]
    fn the_ansi_path_length_counts_gbk_bytes() {
        let g = gbk();
        let fixed = GBK_EXE.len() + VAULT_DIR.len() + 1;
        let fits = "f".repeat(ANSI_PATH_MAX - fixed);
        assert_eq!(
            super::ansi_vault_path(&g, GBK_EXE, None, &fits)
                .unwrap()
                .len(),
            ANSI_PATH_MAX
        );
        // 文字数では259以下でもGBKの2バイトの文字で超える。
        let han = "东".repeat((ANSI_PATH_MAX - fixed) / 2 + 1);
        assert!(han.chars().count() <= ANSI_PATH_MAX - fixed);
        assert!(matches!(
            super::ansi_vault_path(&g, GBK_EXE, None, &han),
            Err(PathError::TooLong(_))
        ));
    }

    fn ini(text: &str) -> Ini {
        crate::ini::parse(text).unwrap()
    }

    #[test]
    fn settings_default_off_and_strict() {
        assert_eq!(settings_from_ini(&ini("")), Ok(None));
        assert_eq!(settings_from_ini(&ini("[netplay]\nenable = 1\n")), Ok(None));
        assert_eq!(settings_from_ini(&ini("[replay]\n")), Ok(None));
        assert_eq!(settings_from_ini(&ini("[replay]\nenable = 0\n")), Ok(None));
        assert_eq!(
            settings_from_ini(&ini("[replay]\nenable = 1\n")),
            Ok(Some(ReplaySettings {
                split: Split::Peer,
                name: "th9_ud{num}".to_string()
            }))
        );
        assert_eq!(
            settings_from_ini(&ini("[Replay]\nenable = 1\nlayout = PACK50\n")),
            Ok(Some(ReplaySettings {
                split: Split::Pack50,
                name: "th9_ud{num}".to_string()
            }))
        );
        // 名前の記法は読むだけで断らない（不適切ならdllが既定の名前で保存して知らせる）。
        assert_eq!(
            settings_from_ini(&ini("[replay]\nenable = 1\nname = a{foo}:b ; 注\n"))
                .unwrap()
                .unwrap()
                .name,
            "a{foo}:b"
        );
        assert!(settings_from_ini(&ini("[replay]\nenable = yes\n")).is_err());
        assert!(settings_from_ini(&ini("[replay]\nenable = 1\nlayuot = date\n")).is_err());
        assert!(settings_from_ini(&ini("[replay]\nenable = 1\nlayout = daily\n")).is_err());
        assert!(settings_from_ini(&ini("[replay]\nenable = 0\nx = 1\n")).is_err());
    }

    /// 偽のメモリ: 設定のコピー（0x1000から）とヘッダ（0x2000から）。書き込めない範囲は`ro`で指定する。
    struct Mem {
        bytes: std::collections::BTreeMap<u32, u8>,
        ro: Vec<u32>,
        writes: Vec<(u32, u8)>,
    }

    impl Mem {
        fn new() -> Mem {
            let mut bytes = std::collections::BTreeMap::new();
            // コピー側: +0xB4 = 1P。+0xB5 = 2P。ヘッダはcfg由来の0 / 0。
            bytes.insert(0x1000 + 0xB4, 1);
            bytes.insert(0x1000 + 0xB5, 0);
            bytes.insert(0x2000 + 0x190, 0);
            bytes.insert(0x2000 + 0x191, 0);
            Mem {
                bytes,
                ro: Vec::new(),
                writes: Vec::new(),
            }
        }

        fn sync(&mut self, copy: Option<u32>, header: Option<u32>) -> ChargeSync {
            let bytes = &mut self.bytes;
            let ro = &self.ro;
            let writes = &mut self.writes;
            // 読みと書きが同じ偽のメモリを触るので読みは呼ぶ前のコピーから取得する。
            let snapshot = bytes.clone();
            sync_header_charge(
                copy,
                header,
                0xB4,
                0x190,
                |a| snapshot.get(&a).copied(),
                |a, v| {
                    if ro.contains(&a) || !bytes.contains_key(&a) {
                        return false;
                    }
                    bytes.insert(a, v);
                    writes.push((a, v));
                    true
                },
            )
        }
    }

    #[test]
    fn charge_goes_to_the_same_seat_in_the_header() {
        let mut m = Mem::new();
        assert_eq!(
            m.sync(Some(0x1000), Some(0x2000)),
            ChargeSync::Written { p1: 1, p2: 0 }
        );
        assert_eq!(m.bytes[&(0x2000 + 0x190)], 1, "1P の欄は写しの 1P");
        assert_eq!(m.bytes[&(0x2000 + 0x191)], 0, "2P の欄は写しの 2P");
        // サイドを入れ替えて見る（2Pだけチャージ型）。
        let mut m = Mem::new();
        m.bytes.insert(0x1000 + 0xB4, 0);
        m.bytes.insert(0x1000 + 0xB5, 1);
        assert_eq!(
            m.sync(Some(0x1000), Some(0x2000)),
            ChargeSync::Written { p1: 0, p2: 1 }
        );
        assert_eq!(
            (m.bytes[&(0x2000 + 0x190)], m.bytes[&(0x2000 + 0x191)]),
            (0, 1)
        );
        // 書くのは2バイトだけ。
        assert_eq!(m.writes, vec![(0x2000 + 0x190, 0), (0x2000 + 0x191, 1)]);
    }

    #[test]
    fn nothing_is_written_when_a_pointer_is_missing_or_unreadable() {
        for (copy, header, want) in [
            (None, Some(0x2000), ChargeSync::NoCopy),
            (Some(0), Some(0x2000), ChargeSync::NoCopy),
            (Some(u32::MAX), Some(0x2000), ChargeSync::NoCopy),
            (Some(0x1000), None, ChargeSync::NoHeader),
            (Some(0x1000), Some(0), ChargeSync::NoHeader),
            (Some(0x1000), Some(u32::MAX), ChargeSync::NoHeader),
            // 指す先が読み取れない
            (Some(0x5000), Some(0x2000), ChargeSync::CopyUnreadable),
        ] {
            let mut m = Mem::new();
            assert_eq!(m.sync(copy, header), want, "{copy:x?} {header:x?}");
            assert!(
                m.writes.is_empty(),
                "{copy:x?} {header:x?}: 書いてはいけない"
            );
        }
    }

    #[test]
    fn an_unwritable_header_is_reported() {
        let mut m = Mem::new();
        m.ro.push(0x2000 + 0x190);
        assert_eq!(m.sync(Some(0x1000), Some(0x2000)), ChargeSync::WriteFailed);
        // ヘッダの先が無い（書き込めない）。
        let mut m = Mem::new();
        assert_eq!(m.sync(Some(0x1000), Some(0x3000)), ChargeSync::WriteFailed);
    }

    #[test]
    fn charge_sync_survives_the_record_word() {
        for c in [
            ChargeSync::NotTried,
            ChargeSync::Written { p1: 1, p2: 0 },
            ChargeSync::Written { p1: 0, p2: 1 },
            ChargeSync::NoCopy,
            ChargeSync::NoHeader,
            ChargeSync::CopyUnreadable,
            ChargeSync::WriteFailed,
        ] {
            assert_eq!(ChargeSync::from_word(c.to_word()), Some(c));
        }
        assert_eq!(ChargeSync::from_word(0), Some(ChargeSync::NotTried));
        assert_eq!(ChargeSync::from_word(0xFF), None);
        assert_ne!(
            ChargeSync::Written { p1: 1, p2: 0 }.to_word(),
            ChargeSync::Written { p1: 0, p2: 1 }.to_word()
        );
    }
}
