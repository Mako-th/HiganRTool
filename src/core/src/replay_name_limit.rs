//! 自動保存のリプレイ名の長さの上限（pure。パスの文字列と環境の判定結果から決める）。
//! 上限は保存先のフォルダのパスの長さ・NTFSのファイル名の長さ・Wine上のUTF-8のバイト数のうち最小のもの。
//! 重複時の接尾辞（`_999`）と`.rpy`の分を除いた名前の本体に対する上限。収まらない名前は末尾から切る。

#![forbid(unsafe_code)]

use crate::replay_name::{DUP_LIMIT, EXT, NameError, check_reserved};

/// パス全体の長さ（UTF-16の単位）。`MAX_PATH`の260から終端のNULを引いた値。
pub const PATH_UNITS: usize = 259;
/// ファイル名1つの長さ（UTF-16の単位。NTFSの上限）。
pub const FILE_UNITS: usize = 255;
/// ファイル名1つの長さ（UTF-8のバイト。Linuxの上限。Wine上でだけ数える）。
pub const FILE_BYTES: usize = 255;
/// 切った後の名前の本体がこれより短くなるときはテンプレートを使わない。
pub const MIN_BODY: usize = 8;

/// 重複時に付く`_2`〜`_999`の最大の長さ。
fn suffix_units() -> usize {
    1 + DUP_LIMIT.to_string().len()
}

/// 名前の本体に使える長さ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameLimit {
    /// UTF-16の単位。
    pub units: usize,
    /// UTF-8のバイト。Wine上だけ。
    pub bytes: Option<usize>,
    /// 数えた保存先のフォルダのパスの長さ（末尾の`\`を含むUTF-16の単位）。
    pub dir_units: usize,
    /// 実際のパスより差し替え先（VirtualStore）のパスの方が長く、そちらで数えたかどうか。
    pub virtual_counted: bool,
}

fn units_of(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `/`を`\`にし、末尾の`\`を取る。
fn normal(path: &str) -> String {
    let p = path.replace('/', "\\");
    let p = p.strip_prefix("\\\\?\\").unwrap_or(&p);
    p.trim_end_matches('\\').to_string()
}

fn is_under(dir: &str, root: &str) -> bool {
    if root.is_empty() {
        return false;
    }
    let (d, r) = (dir.to_lowercase(), root.to_lowercase());
    d == r || d.starts_with(&format!("{r}\\"))
}

/// `path`の差し替え先`<local_app_data>\VirtualStore\<ドライブ文字を除いた path>`。
/// `local_app_data`が空・`path`にドライブ文字が無い（UNCなど）ときは`None`。
fn virtual_path(path: &str, local_app_data: &str) -> Option<String> {
    let d = normal(path);
    let local = normal(local_app_data);
    if local.is_empty() {
        return None;
    }
    let mut it = d.chars();
    let (drive, colon) = (it.next()?, it.next()?);
    if !drive.is_ascii_alphabetic() || colon != ':' {
        return None;
    }
    let rest = &d[2..];
    if !rest.starts_with('\\') {
        return None;
    }
    Some(format!("{local}\\VirtualStore{rest}"))
}

/// `dir`が保護された場所（`protected_roots`のどれかの下）ならWindowsが書き先を差し替えることがある。
/// その差し替え先`<local_app_data>\VirtualStore\<ドライブ文字を除いた dir>`を返す。保護された場所の外・ドライブ文字の無いパスは`None`。
/// 差し替えは書き込みの権限が無いとき（推定）だけだが、長さは安全側で常に両方を見る。
pub fn virtual_store_dir(
    dir: &str,
    protected_roots: &[String],
    local_app_data: &str,
) -> Option<String> {
    if !protected_roots
        .iter()
        .any(|r| is_under(&normal(dir), &normal(r)))
    {
        return None;
    }
    virtual_path(dir, local_app_data)
}

/// 保存したファイルがVirtualStoreへ差し替えられたかどうか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualCopy {
    /// 差し替え先に同じファイルがある（保存したパスを開くと差し替え先が開く）。
    Present,
    /// 差し替え先にファイルが無い。
    Absent,
    /// 差し替え先に同名のファイルがあるが、保存したファイルとは別のもの。
    OtherWithSameName,
    /// 差し替え先のパスを作れない・存在や同一性を調べられなかった。
    Unknown,
}

/// 保存したファイル`saved`が差し替え先にあるかを決める。
/// `exists`は差し替え先のパスを受けて存在を返す。
/// `same`は差し替え先に在るときだけ呼ばれ、保存したパスと差し替え先が同じファイルかを返す（どちらも調べられなければ`None`）。
/// OSが開き先を黙って差し替えるため、保存したパスの存在では見分けられない。差し替え先を直接調べる。
pub fn virtual_copy(
    saved: &str,
    local_app_data: &str,
    exists: impl FnOnce(&str) -> Option<bool>,
    same: impl FnOnce(&str) -> Option<bool>,
) -> VirtualCopy {
    let Some(v) = virtual_path(saved, local_app_data) else {
        return VirtualCopy::Unknown;
    };
    match exists(&v) {
        Some(false) => VirtualCopy::Absent,
        Some(true) => match same(&v) {
            Some(true) => VirtualCopy::Present,
            Some(false) => VirtualCopy::OtherWithSameName,
            None => VirtualCopy::Unknown,
        },
        None => VirtualCopy::Unknown,
    }
}

/// 自動保存が済んだときに本ログへ出す1行。`file`・`place`は呼び出し側が伏せた後の文字列。
pub fn saved_line(file: &str, place: &str, copy: VirtualCopy) -> String {
    let copy = match copy {
        VirtualCopy::Present => "あり",
        VirtualCopy::Absent => "なし",
        VirtualCopy::OtherWithSameName => "なし・VirtualStore に同名の別のファイルあり",
        VirtualCopy::Unknown => "確認不能",
    };
    format!("自動保存: {file} を保存しました（保存先 {place}・VirtualStore への差し替え {copy}）")
}

/// 保存先のフォルダ`dir`（と差し替え先`virtual_dir`）から名前の上限を決める。
/// 260文字の制限が差し替え前と後のどちらのパスにかかるかは未確認なので長い方で数える。
pub fn name_limit(dir: &str, virtual_dir: Option<&str>, wine: bool) -> NameLimit {
    let real = units_of(&normal(dir)) + 1;
    let virt = virtual_dir.map(|v| units_of(&normal(v)) + 1);
    let (dir_units, virtual_counted) = match virt {
        Some(v) if v > real => (v, true),
        _ => (real, false),
    };
    let reserve = EXT.len() + suffix_units();
    let total = PATH_UNITS.saturating_sub(dir_units).min(FILE_UNITS);
    NameLimit {
        units: total.saturating_sub(reserve),
        bytes: wine.then(|| FILE_BYTES - reserve),
        dir_units,
        virtual_counted,
    }
}

impl NameLimit {
    /// 保存のたびに本ログへ出す1行。パスそのものは書かず長さだけ。
    pub fn describe(&self) -> String {
        let mut what = format!("{} 文字", self.units);
        if let Some(b) = self.bytes {
            what.push_str(&format!("・UTF-8 で {b} バイト"));
        }
        let mut note = format!("保存先のフォルダ {} 文字", self.dir_units);
        if self.virtual_counted {
            note.push_str("。VirtualStore で計算");
        }
        if self.bytes.is_some() {
            note.push_str("。Wine でバイト数も計算");
        }
        format!("リプレイ名の上限: {what}（{note}）")
    }
}

/// 上限に収めた名前。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fitted {
    pub name: String,
    /// `name`のUTF-16の単位。
    pub units: usize,
    /// 切ったときの、切る前のUTF-16の単位。
    pub cut_from: Option<usize>,
}

impl Fitted {
    /// 切ったときだけ本ログへ出す1行。
    pub fn describe(&self) -> Option<String> {
        let from = self.cut_from?;
        Some(format!(
            "リプレイ名: {from} 文字を {} 文字に切り捨てました。",
            self.units
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FitError {
    /// 切った結果が`MIN_BODY`文字に満たない（保存先のフォルダが長すぎる）。
    TooNarrow {
        limit: usize,
    },
    Name(NameError),
}

impl FitError {
    pub fn reason(&self) -> String {
        match self {
            FitError::TooNarrow { limit } => {
                format!("保存先のフォルダが長すぎます（名前の上限 {limit} 文字）")
            }
            FitError::Name(e) => e.reason(),
        }
    }
}

/// `stem`（`.rpy`と重複の末尾を除いた名前）が`limit`で切られた長さかどうか。
/// `fit`は次の1文字（UTF-16で2単位・UTF-8で4バイトまで）が入らない所で切るのでその幅の分だけ短くても当てる。
pub fn is_cut_length(stem: &str, limit: &NameLimit) -> bool {
    units_of(stem) + 2 > limit.units || limit.bytes.is_some_and(|m| stem.len() + 4 > m)
}

/// 展開した名前`base`（`.rpy`を除く）を`limit`に収める。サロゲートペアとUTF-8の文字は途中で割らない。
/// 切った後に予約の名前になるものは断る。
pub fn fit(base: &str, limit: &NameLimit) -> Result<Fitted, FitError> {
    let total = units_of(base);
    let mut name = String::new();
    let (mut units, mut bytes) = (0, 0);
    for c in base.chars() {
        let (u, b) = (c.len_utf16(), c.len_utf8());
        if units + u > limit.units || limit.bytes.is_some_and(|m| bytes + b > m) {
            break;
        }
        name.push(c);
        units += u;
        bytes += b;
    }
    let cut = units < total;
    if cut && units < MIN_BODY {
        return Err(FitError::TooNarrow { limit: limit.units });
    }
    check_reserved(&name).map_err(FitError::Name)?;
    Ok(Fitted {
        name,
        units,
        cut_from: cut.then_some(total),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_name::candidate_file;

    const LOCAL: &str = "C:\\Homes\\testuser\\AppData\\Local";
    const INSTALL: &str = "C:\\Program Files (x86)\\上海アリス幻樂団\\東方花映塚";

    fn roots() -> Vec<String> {
        vec![
            "C:\\Program Files".to_string(),
            "C:\\Program Files (x86)".to_string(),
            "C:\\Windows".to_string(),
        ]
    }

    /// 既定のインストール先の保管庫の相手ごとのフォルダ。名前は合成（ASCII 32文字ずつ）。
    fn peer_dir(root: &str, p1: &str, p2: &str) -> String {
        format!("{root}\\Replay_HiganRTool\\{p1} vs {p2}")
    }

    fn limit_of(dir: &str, wine: bool) -> NameLimit {
        let v = virtual_store_dir(dir, &roots(), LOCAL);
        name_limit(dir, v.as_deref(), wine)
    }

    #[test]
    fn a_protected_place_maps_to_virtual_store_without_the_drive() {
        let v = virtual_store_dir(INSTALL, &roots(), LOCAL).unwrap();
        assert_eq!(
            v,
            format!("{LOCAL}\\VirtualStore\\Program Files (x86)\\上海アリス幻樂団\\東方花映塚")
        );
    }

    #[test]
    fn the_protected_place_match_ignores_case_slashes_and_trailing_separators() {
        let want = format!("{LOCAL}\\VirtualStore\\Windows\\Games\\x").to_lowercase();
        for d in [
            "C:\\Windows\\Games\\x",
            "c:\\WINDOWS\\games\\x\\",
            "C:/Windows/Games/x",
            "\\\\?\\C:\\Windows\\Games\\x",
        ] {
            let got = virtual_store_dir(d, &roots(), &format!("{LOCAL}\\"));
            assert_eq!(got.map(|s| s.to_lowercase()), Some(want.clone()), "{d}");
        }
        // 末尾の`\`付きのルートも同じ。
        let r = vec!["C:\\Windows\\".to_string()];
        assert!(virtual_store_dir("C:\\Windows\\a", &r, LOCAL).is_some());
        assert!(virtual_store_dir("C:\\Windows", &r, LOCAL).is_some());
    }

    #[test]
    fn places_outside_the_protected_roots_are_not_mapped() {
        for d in [
            "D:\\Games\\th09",
            "C:\\Program Files (x86)Evil\\a",
            "C:\\Windows2\\a",
            "C:\\Homes\\testuser\\Games",
            "\\\\server\\share\\Program Files\\a",
            "relative\\Program Files",
        ] {
            assert_eq!(virtual_store_dir(d, &roots(), LOCAL), None, "{d}");
        }
        assert_eq!(virtual_store_dir(INSTALL, &[], LOCAL), None);
        assert_eq!(virtual_store_dir(INSTALL, &roots(), ""), None);
        assert_eq!(
            virtual_store_dir(INSTALL, &["".to_string()], LOCAL),
            None,
            "空の根は何にも当たらない"
        );
    }

    #[test]
    fn the_longer_of_the_real_and_virtual_paths_is_counted() {
        let dir = "C:\\Program Files\\g";
        let v = "C:\\Homes\\testuser\\AppData\\Local\\VirtualStore\\Program Files\\g";
        let l = name_limit(dir, Some(v), false);
        assert!(l.virtual_counted);
        assert_eq!(l.dir_units, v.len() + 1);
        // 差し替え先の方が短ければ実際のパスで数える。
        let l = name_limit(v, Some(dir), false);
        assert!(!l.virtual_counted);
        assert_eq!(l.dir_units, v.len() + 1);
        // 差し替えの対象でなければ実際のパスだけ。
        let l = name_limit(dir, None, false);
        assert!(!l.virtual_counted);
        assert_eq!(l.dir_units, dir.len() + 1);
    }

    #[test]
    fn the_limit_leaves_room_for_the_suffix_and_the_extension() {
        let l = name_limit("C:", None, false);
        assert_eq!(l.dir_units, 3);
        // 短いパスではファイル名の255単位が効く。255 - `_999`の4 - `.rpy`の4。
        assert_eq!(l.units, 247);
        assert_eq!(l.bytes, None);
        // 長いパスではパス全体の259が効く。
        let dir = format!("C:\\{}", "d".repeat(117));
        let l = name_limit(&dir, None, false);
        assert_eq!(l.dir_units, 121);
        assert_eq!(l.units, 259 - 121 - 8);
        let name = "a".repeat(l.units);
        let f = fit(&name, &l).unwrap();
        for k in [1, 2, 999] {
            let file = candidate_file(&f.name, k);
            assert!(l.dir_units + units_of(&file) <= PATH_UNITS, "{k}");
        }
        assert_eq!(l.dir_units + units_of(&candidate_file(&f.name, 999)), 259);
    }

    #[test]
    fn wine_adds_a_utf8_byte_limit_and_windows_does_not() {
        assert_eq!(name_limit("C:\\g", None, true).bytes, Some(247));
        assert_eq!(name_limit("C:\\g", None, false).bytes, None);
        let jp = "日".repeat(200);
        let win = fit(&jp, &name_limit("C:\\g", None, false)).unwrap();
        assert_eq!(win.units, 200, "Windowsは単位で数える");
        let wine = fit(&jp, &name_limit("C:\\g", None, true)).unwrap();
        // 247 / 3 = 82文字（246バイト）。文字の途中では切らない。
        assert_eq!(wine.units, 82);
        assert_eq!(wine.name.len(), 246);
        assert_eq!(wine.cut_from, Some(200));
    }

    #[test]
    fn a_surrogate_pair_is_never_split() {
        let l = NameLimit {
            units: 9,
            bytes: None,
            dir_units: 10,
            virtual_counted: false,
        };
        // 8単位の次の絵文字は2単位で9に収まらない。
        let f = fit(&format!("{}😀tail", "a".repeat(8)), &l).unwrap();
        assert_eq!(f.name, "a".repeat(8));
        assert_eq!(f.cut_from, Some(8 + 2 + 4));
        // 7単位の次なら収まる。
        let f = fit(&format!("{}😀tail", "a".repeat(7)), &l).unwrap();
        assert_eq!(f.name, format!("{}😀", "a".repeat(7)));
        assert_eq!(f.units, 9);
        assert_eq!(f.name.encode_utf16().count(), 9);
    }

    #[test]
    fn a_name_that_fits_is_kept_and_reports_no_cut() {
        let l = name_limit("C:\\g", None, false);
        let f = fit("abc", &l).unwrap();
        assert_eq!(
            (f.name.as_str(), f.cut_from, f.describe()),
            ("abc", None, None)
        );
        let exact = "x".repeat(l.units);
        assert_eq!(fit(&exact, &l).unwrap().cut_from, None);
        let over = "x".repeat(l.units + 1);
        assert_eq!(fit(&over, &l).unwrap().cut_from, Some(l.units + 1));
    }

    #[test]
    fn a_folder_that_is_too_long_refuses_the_template() {
        // 空きが7単位（名前の本体がMIN_BODY - 1）。
        let dir = format!("C:\\{}", "d".repeat(259 - 8 - 7 - 4));
        let l = name_limit(&dir, None, false);
        assert_eq!(l.units, 7);
        assert_eq!(fit("abcdefghij", &l), Err(FitError::TooNarrow { limit: 7 }));
        // 収まる短い名前なら断らない。
        assert!(fit("abcdefg", &l).is_ok());
        // 8単位ならちょうど使える。
        let dir = format!("C:\\{}", "d".repeat(259 - 8 - 8 - 4));
        let l = name_limit(&dir, None, false);
        assert_eq!(fit("abcdefghij", &l).unwrap().name, "abcdefgh");
        // パスが259を越えていてもパニックしない。
        let l = name_limit(&format!("C:\\{}", "d".repeat(400)), None, false);
        assert_eq!(l.units, 0);
        assert_eq!(fit("abc", &l), Err(FitError::TooNarrow { limit: 0 }));
        assert!(
            FitError::TooNarrow { limit: 7 }
                .reason()
                .contains("上限 7 文字")
        );
    }

    #[test]
    fn a_name_that_becomes_reserved_after_the_cut_is_refused() {
        let l = NameLimit {
            units: 10,
            bytes: None,
            dir_units: 10,
            virtual_counted: false,
        };
        assert_eq!(
            fit("con       x", &l),
            Err(FitError::Name(NameError::Reserved))
        );
        assert!(fit("con      xx", &l).is_ok());
        assert_eq!(
            FitError::Name(NameError::Reserved).reason(),
            "Windows の予約名"
        );
    }

    #[test]
    fn the_log_lines_give_lengths_but_no_paths() {
        let l = limit_of(&peer_dir(INSTALL, &"a".repeat(32), &"b".repeat(32)), false);
        assert_eq!(
            l.describe(),
            "リプレイ名の上限: 84 文字（保存先のフォルダ 167 文字。VirtualStore で計算）"
        );
        let l = limit_of("D:", false);
        assert_eq!(
            l.describe(),
            "リプレイ名の上限: 247 文字（保存先のフォルダ 3 文字）"
        );
        let l = limit_of("D:", true);
        assert_eq!(
            l.describe(),
            "リプレイ名の上限: 247 文字・UTF-8 で 247 バイト（保存先のフォルダ 3 文字。Wine でバイト数も計算）"
        );
        let f = fit(&"a".repeat(300), &name_limit("C:", None, false)).unwrap();
        assert_eq!(
            f.describe().as_deref(),
            Some("リプレイ名: 300 文字を 247 文字に切り捨てました。")
        );
    }

    /// 配布する`HiganRTool.ini`の「約80文字」の根拠。
    /// 既定のインストール先のVirtualStore側（ユーザー名8文字）・プレイヤー名2人とも上限の32文字。
    #[test]
    fn the_default_install_with_32_character_names_gives_84_characters() {
        let local = "C:\\Homes\\12345678\\AppData\\Local";
        let dir = peer_dir(INSTALL, &"a".repeat(32), &"b".repeat(32));
        let v = virtual_store_dir(&dir, &roots(), local).unwrap();
        let l = name_limit(&dir, Some(&v), false);
        assert!(l.virtual_counted);
        // C:\Homes\12345678\AppData\Local(31) + \VirtualStore(13) + \Program Files (x86)(20)
        // + \上海アリス幻樂団(9) + \東方花映塚(6) + \Replay_HiganRTool(18) + \<32> vs <32>(69) + 末尾の\(1)
        assert_eq!(l.dir_units, 31 + 13 + 20 + 9 + 6 + 18 + 69 + 1);
        assert_eq!(l.units, 259 - 167 - 8);
        assert_eq!(l.units, 84);
    }

    /// 実際の名前と同じ形の合成の名前。実在の名前は使わない。
    fn synthetic_names(limit: usize) -> Vec<String> {
        vec![
            "n".repeat(limit),
            "全".repeat(limit),
            "全角abc".repeat(limit),
            "a_b-c d&e ".repeat(limit),
            format!("{} {}", "_".repeat(limit), "-".repeat(limit)),
            format!("{}{}", "あ".repeat(limit / 2), "x".repeat(limit)),
            "😀".repeat(limit),
            "&".repeat(limit + 1),
        ]
    }

    #[test]
    fn synthetic_names_fit_in_the_path_and_in_the_utf8_limit() {
        let ascii = ("a".repeat(32), "b".repeat(32));
        let wide = ("全".repeat(16), "角".repeat(16));
        let dirs = [
            peer_dir(INSTALL, &ascii.0, &ascii.1),
            peer_dir(INSTALL, &wide.0, &wide.1),
            peer_dir(
                &format!("C:\\Games\\12345678\\Plays\\{}", "深".repeat(60)),
                &ascii.0,
                &ascii.1,
            ),
            "D:\\g".to_string(),
        ];
        for dir in &dirs {
            for wine in [false, true] {
                let l = limit_of(dir, wine);
                for base in synthetic_names(l.units) {
                    let Ok(f) = fit(&base, &l) else {
                        panic!("{dir} / {base}");
                    };
                    for k in [1, 2, 999] {
                        let file = candidate_file(&f.name, k);
                        assert!(
                            l.dir_units + units_of(&file) <= PATH_UNITS,
                            "{dir} {k}: {}",
                            l.dir_units + units_of(&file)
                        );
                        assert!(units_of(&file) <= FILE_UNITS);
                        if wine {
                            assert!(file.len() <= FILE_BYTES, "{dir} {k}: {}", file.len());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_exact_limit_name_is_not_cut_and_one_more_is() {
        let ascii = ("a".repeat(32), "b".repeat(32));
        let l = limit_of(&peer_dir(INSTALL, &ascii.0, &ascii.1), false);
        let exact = "y".repeat(l.units);
        let f = fit(&exact, &l).unwrap();
        assert_eq!(f.cut_from, None);
        assert_eq!(l.dir_units + units_of(&candidate_file(&f.name, 999)), 259);
        let f = fit(&format!("{exact}y"), &l).unwrap();
        assert_eq!(f.cut_from, Some(l.units + 1));
        assert_eq!(f.name, exact);
    }

    /// 差し替え先の有無と同一性の組み合わせごとの結果。`same`は差し替え先に在るときしか呼ばれない。
    #[test]
    fn the_virtual_copy_is_decided_from_the_virtual_path_alone() {
        let saved = "C:\\Program Files (x86)\\Game\\Vault\\a.rpy";
        let want = format!("{LOCAL}\\VirtualStore\\Program Files (x86)\\Game\\Vault\\a.rpy");
        let at = |ex: Option<bool>, same: Option<bool>| {
            let (mut asked, mut asked_same) = (Vec::new(), 0);
            let got = virtual_copy(
                saved,
                LOCAL,
                |p| {
                    asked.push(p.to_string());
                    ex
                },
                |_| {
                    asked_same += 1;
                    same
                },
            );
            (got, asked, asked_same)
        };
        let ask = vec![want.clone()];
        assert_eq!(
            at(Some(true), Some(true)),
            (VirtualCopy::Present, ask.clone(), 1)
        );
        assert_eq!(
            at(Some(true), Some(false)),
            (VirtualCopy::OtherWithSameName, ask.clone(), 1)
        );
        assert_eq!(at(Some(true), None), (VirtualCopy::Unknown, ask.clone(), 1));
        assert_eq!(
            at(Some(false), Some(true)),
            (VirtualCopy::Absent, ask.clone(), 0)
        );
        assert_eq!(at(None, Some(true)), (VirtualCopy::Unknown, ask, 0));
    }

    /// 保護された場所の判定を使わない。LOCALAPPDATAが空・ドライブ文字が無いときは調べずに確認不能。
    #[test]
    fn the_virtual_copy_is_unknown_without_a_virtual_path() {
        let never = |_: &str| -> Option<bool> { panic!("調べてはいけない") };
        assert_eq!(
            virtual_copy("D:\\Games\\Vault\\a.rpy", "", never, never),
            VirtualCopy::Unknown
        );
        assert_eq!(
            virtual_copy("\\\\host\\share\\a.rpy", LOCAL, never, never),
            VirtualCopy::Unknown
        );
        // 保護された場所の外でも差し替え先は調べる（無ければなし）。
        assert_eq!(
            virtual_copy("D:\\Games\\a.rpy", LOCAL, |_| Some(false), never),
            VirtualCopy::Absent
        );
    }

    #[test]
    fn the_saved_line_names_each_virtual_copy_state() {
        let line = |c| saved_line("th9_ud0001.rpy", "Replay_HiganRTool", c);
        assert_eq!(
            line(VirtualCopy::Present),
            "自動保存: th9_ud0001.rpy を保存しました（保存先 Replay_HiganRTool・VirtualStore への差し替え あり）"
        );
        assert!(line(VirtualCopy::Absent).ends_with("差し替え なし）"));
        assert!(
            line(VirtualCopy::OtherWithSameName)
                .ends_with("差し替え なし・VirtualStore に同名の別のファイルあり）")
        );
        assert!(line(VirtualCopy::Unknown).ends_with("差し替え 確認不能）"));
    }
}
