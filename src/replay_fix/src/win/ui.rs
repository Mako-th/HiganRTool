//! 窓。Windows 標準の部品（BUTTON・COMBOBOX・STATIC）で作った、普通のダイアログに近い窓。
//! 見た目はコモンコントロール v6 のマニフェスト（build.rs）に、Tab・Enter・Esc は `IsDialogMessageW` に任せる。
//! 窓のクラスは `DefDlgProcW` を既定の処理にした「ダイアログ窓」で、既定のボタンの管理・フォーカスの戻し・背景色を OS が受け持つ。
//! 自分の窓の部品へ送るメッセージ（WM_SETFONT / CB_*）にだけ `SendMessageW` を使う。他のプロセスの窓へは送らない。
//! 状態（何本目か・直した後の設定・押せるか）は `model`、文言は `text`、読み書きは `work` / `scan`。ここは部品の並べ方と入力の橋渡し。

use core::ffi::c_void;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::ptr;

use higanr_core::lang::Lang;
use higanr_core::replay::Side;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    COLOR_3DFACE, COLOR_BTNTEXT, CreateFontIndirectW, GetDC, GetDeviceCaps, GetSysColor,
    GetSysColorBrush, GetTextExtentPoint32W, HDC, HFONT, HGDIOBJ, LOGPIXELSX, ReleaseDC,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT, UpdateWindow,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::{
    SS_CENTER, SS_ENDELLIPSIS, SS_ETCHEDHORZ, SS_LEFT, SS_NOPREFIX, SS_PATHELLIPSIS,
};
use windows_sys::Win32::UI::Controls::{
    ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetFocus, IsWindowEnabled, SetFocus,
};
use windows_sys::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, BN_CLICKED, BS_PUSHBUTTON, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL,
    CBN_SELCHANGE, CBS_DROPDOWNLIST, CW_USEDEFAULT, CreateWindowExW, DLGWINDOWEXTRA, DM_SETDEFID,
    DefDlgProcW, DestroyWindow, DispatchMessageW, GetMessageW, IDC_ARROW, IDC_WAIT, IDCANCEL, IDOK,
    IsChild, IsDialogMessageW, IsWindowVisible, LoadCursorW, MB_ICONINFORMATION, MB_OK, MSG,
    MessageBoxW, MoveWindow, NONCLIENTMETRICSW, PostQuitMessage, RegisterClassExW,
    SPI_GETNONCLIENTMETRICS, SW_HIDE, SW_SHOW, SendMessageW, SetCursor, SetProcessDPIAware,
    SetWindowTextW, ShowWindow, SystemParametersInfoW, TranslateMessage, WM_CLOSE, WM_COMMAND,
    WM_CTLCOLORSTATIC, WM_DESTROY, WM_DROPFILES, WM_SETFONT, WNDCLASSEXW, WS_CAPTION, WS_CHILD,
    WS_CLIPCHILDREN, WS_EX_ACCEPTFILES, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP,
    WS_VSCROLL,
};

use higanrtool_replay_fix::model::{Analysis, Kind, MakeState, Notice, Player, Session};
use higanrtool_replay_fix::scan::collect;
use higanrtool_replay_fix::text::{Text, lang_name};
use higanrtool_replay_fix::work::{MakeResult, Report, analyze, make, make_all};

use super::dialogs;

const CLASS_NAME: &str = "HiganRToolReplayFix";

/// 論理ピクセル（96 dpi）での寸法。ボタンの高さ 23・最小幅 75・窓の縁の余白 12 は Windows のダイアログの標準
/// （ダイアログ単位の 14 と 50 と 7 を 96 dpi に直した値）。
const CLIENT_W: i32 = 560;
const CLIENT_H: i32 = 323;
const MARGIN: i32 = 12;
const BUTTON_H: i32 = 23;
const BUTTON_MIN_W: i32 = 75;
const BUTTON_PAD: i32 = 24;
const BUTTON_GAP: i32 = 6;
const BUTTON_GAP_EXIT: i32 = 18;
const SEPARATOR_Y: i32 = 276;
const BUTTON_Y: i32 = 288;
/// 選択欄の閉じた高さ（文字に合わせて OS が決める）の目安と、開いたときの一覧の高さ。
const COMBO_H: i32 = 23;
const COMBO_LIST_H: i32 = 66;

/// 部品の識別番号。`Make` と `Close` は Enter と Esc に当たる標準の番号（`IDOK` / `IDCANCEL`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Id {
    ChooseReplays,
    ChooseFolder,
    Close,
    Prev,
    Next,
    Make,
    MakeAll,
    Language,
    Drop(Player),
}

impl Id {
    fn ctl(self) -> i32 {
        match self {
            Id::Make => IDOK,
            Id::Close => IDCANCEL,
            Id::ChooseReplays => 101,
            Id::ChooseFolder => 102,
            Id::Prev => 103,
            Id::Next => 104,
            Id::MakeAll => 105,
            Id::Language => 106,
            Id::Drop(Player::P1) => 107,
            Id::Drop(Player::P2) => 108,
        }
    }

    fn from_ctl(v: i32) -> Option<Id> {
        [
            Id::ChooseReplays,
            Id::ChooseFolder,
            Id::Close,
            Id::Prev,
            Id::Next,
            Id::Make,
            Id::MakeAll,
            Id::Language,
            Id::Drop(Player::P1),
            Id::Drop(Player::P2),
        ]
        .into_iter()
        .find(|id| id.ctl() == v)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Start,
    Detail,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Normal,
    Good,
    Bad,
}

/// 知らせの行の色。`WM_CTLCOLORSTATIC` は部品の作り直しの最中にも届くので、状態の借用とは別に持つ。
#[derive(Clone, Copy)]
struct MessageStyle {
    hwnd: usize,
    color: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static MESSAGE_STYLE: Cell<MessageStyle> = const { Cell::new(MessageStyle { hwnd: 0, color: 0 }) };
}

/// 窓の中の部品。作る順が Tab の順（言語は最後）。
struct Controls {
    path: HWND,
    lang_label: HWND,
    lang_combo: HWND,
    hint: HWND,
    heads: [HWND; 2],
    /// 側 / 名前 / 今の設定 / 直した後の設定
    row_labels: [HWND; 4],
    /// 側 / 名前 / 今の設定 の 1P と 2P
    cells: [[HWND; 2]; 3],
    drops: [HWND; 2],
    message: HWND,
    pos: HWND,
    separator: HWND,
    prev: HWND,
    next: HWND,
    choose_replays: HWND,
    choose_folder: HWND,
    make: HWND,
    make_all: HWND,
    close: HWND,
}

struct App {
    hwnd: HWND,
    lang: Lang,
    dpi: i32,
    font: HFONT,
    screen: Screen,
    /// 部品を並べた画面。切り替わったときだけ既定のボタンとフォーカスを置き直す。
    shown: Option<Screen>,
    session: Option<Session>,
    notice: Option<Notice>,
    c: Controls,
}

/// 状態を借りて使う。借りられない（窓のメッセージの途中の再入）ときは `None` で、パニックしない。
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|c| {
        let mut b = c.try_borrow_mut().ok()?;
        b.as_mut().map(f)
    })
}

fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}

fn tone_color(tone: Tone) -> u32 {
    match tone {
        // SAFETY: システムの色を読むだけ。
        Tone::Normal => unsafe { GetSysColor(COLOR_BTNTEXT) },
        Tone::Good => rgb(16, 124, 16),
        Tone::Bad => rgb(176, 0, 32),
    }
}

// ---- Win32 の薄い包み ----

fn system_font() -> HFONT {
    // SAFETY: `ncm` は cbSize を設定した書ける構造体。取れなければ 0 のまま（既定のフォントで作る）。
    unsafe {
        let mut ncm: NONCLIENTMETRICSW = core::mem::zeroed();
        ncm.cbSize = core::mem::size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            ncm.cbSize,
            &mut ncm as *mut _ as *mut c_void,
            0,
        );
        ncm.lfMessageFont.lfCharSet = 1; // DEFAULT_CHARSET: 日本語など、顔の文字集合に無い字も OS の代替で出す
        CreateFontIndirectW(&ncm.lfMessageFont)
    }
}

fn set_text(h: HWND, text: &str) {
    let w = wide_nul(text);
    // SAFETY: `h` は自分の窓の部品。文字列は終端 0 つきで呼びの間生きている。
    unsafe { SetWindowTextW(h, w.as_ptr()) };
}

fn place(h: HWND, x: i32, y: i32, w: i32, height: i32) {
    // SAFETY: `h` は自分の窓の部品。
    unsafe {
        MoveWindow(h, x, y, w, height, 1);
        ShowWindow(h, SW_SHOW);
    }
}

fn hide(h: HWND) {
    // SAFETY: `h` は自分の窓の部品。
    unsafe { ShowWindow(h, SW_HIDE) };
}

fn enable(h: HWND, on: bool) {
    // SAFETY: `h` は自分の窓の部品。
    unsafe { EnableWindow(h, i32::from(on)) };
}

fn is_enabled(h: HWND) -> bool {
    // SAFETY: `h` は自分の窓の部品。
    unsafe { IsWindowEnabled(h) != 0 }
}

/// 自分の窓の部品へのメッセージ。他のプロセスの窓へは送らない。
fn send(h: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: `h` は自分の窓か、その部品。`lparam` に渡すポインタは呼び出し側が呼びの間生かす。
    unsafe { SendMessageW(h, msg, wparam, lparam) }
}

fn combo_add(h: HWND, text: &str) {
    let w = wide_nul(text);
    send(h, CB_ADDSTRING, 0, w.as_ptr() as LPARAM);
}

fn combo_select(h: HWND, index: Option<usize>) {
    // CB_SETCURSEL の -1 は「選ばない」。
    send(h, CB_SETCURSEL, index.unwrap_or(usize::MAX) as WPARAM, 0);
}

fn combo_selected(h: HWND) -> Option<usize> {
    usize::try_from(send(h, CB_GETCURSEL, 0, 0)).ok()
}

fn child(parent: HWND, class: &str, style: u32, id: i32, font: HFONT, hinst: *mut c_void) -> HWND {
    let c = wide_nul(class);
    let empty = wide_nul("");
    // SAFETY: 文字列は終端 0 つきで呼びの間生きている。`parent` は作った自分の窓。識別番号は HMENU の欄で渡す決まり。
    let h = unsafe {
        CreateWindowExW(
            0,
            c.as_ptr(),
            empty.as_ptr(),
            WS_CHILD | style,
            0,
            0,
            0,
            0,
            parent,
            id as usize as *mut c_void,
            hinst,
            ptr::null(),
        )
    };
    send(h, WM_SETFONT, font as WPARAM, 1);
    h
}

impl Controls {
    fn create(parent: HWND, font: HFONT, hinst: *mut c_void) -> Controls {
        let stat = |style: u32| child(parent, "STATIC", style, -1, font, hinst);
        let label = SS_LEFT | SS_NOPREFIX;
        let cell = SS_LEFT | SS_NOPREFIX | SS_ENDELLIPSIS;
        // 既定のボタンの印は、画面ごとに DM_SETDEFID で OS が付け替える。
        let button = |id: Id| {
            child(
                parent,
                "BUTTON",
                WS_TABSTOP | BS_PUSHBUTTON as u32,
                id.ctl(),
                font,
                hinst,
            )
        };
        let combo = |id: Id| {
            child(
                parent,
                "COMBOBOX",
                WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
                id.ctl(),
                font,
                hinst,
            )
        };
        let path = stat(SS_LEFT | SS_NOPREFIX | SS_PATHELLIPSIS);
        let lang_label = stat(label);
        let hint = stat(label);
        let heads = [stat(label), stat(label)];
        let row_labels = [stat(label), stat(label), stat(label), stat(label)];
        let cells = [
            [stat(cell), stat(cell)],
            [stat(cell), stat(cell)],
            [stat(cell), stat(cell)],
        ];
        let message = stat(label);
        let pos = stat(SS_CENTER | SS_NOPREFIX);
        let separator = stat(SS_ETCHEDHORZ);
        // 以降は Tab の順。
        let drops = [combo(Id::Drop(Player::P1)), combo(Id::Drop(Player::P2))];
        let prev = button(Id::Prev);
        let next = button(Id::Next);
        let choose_replays = button(Id::ChooseReplays);
        let choose_folder = button(Id::ChooseFolder);
        let make = button(Id::Make);
        let make_all = button(Id::MakeAll);
        let close = button(Id::Close);
        let lang_combo = combo(Id::Language);
        for h in drops {
            for k in Kind::ALL {
                combo_add(h, k.name());
            }
        }
        for l in Lang::ALL {
            combo_add(lang_combo, lang_name(l));
        }
        Controls {
            path,
            lang_label,
            lang_combo,
            hint,
            heads,
            row_labels,
            cells,
            drops,
            message,
            pos,
            separator,
            prev,
            next,
            choose_replays,
            choose_folder,
            make,
            make_all,
            close,
        }
    }

    /// 1 本ずつの画面でだけ出す部品。
    fn detail_only(&self) -> Vec<HWND> {
        let mut v = vec![self.path, self.pos, self.prev, self.next, self.make];
        v.extend([self.make_all, self.drops[0], self.drops[1]]);
        v.extend(self.heads);
        v.extend(self.row_labels);
        v.extend(self.cells.iter().flatten().copied());
        v
    }

    /// 最初の画面でだけ出す部品。
    fn start_only(&self) -> [HWND; 3] {
        [self.hint, self.choose_replays, self.choose_folder]
    }
}

impl App {
    fn sc(&self, v: i32) -> i32 {
        v * self.dpi / 96
    }

    fn text(&self) -> Text {
        Text(self.lang)
    }

    fn text_width(&self, text: &str) -> i32 {
        let w: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE { cx: 0, cy: 0 };
        // SAFETY: 窓の DC を借りてフォントを選び、元へ戻して返す。`w` は長さ付きで、`size` は書ける。
        unsafe {
            let dc: HDC = GetDC(self.hwnd);
            let old = SelectObject(dc, self.font as HGDIOBJ);
            GetTextExtentPoint32W(dc, w.as_ptr(), w.len() as i32, &mut size);
            SelectObject(dc, old);
            ReleaseDC(self.hwnd, dc);
        }
        size.cx
    }

    fn button_width(&self, text: &str) -> i32 {
        (self.text_width(text) + self.sc(BUTTON_PAD)).max(self.sc(BUTTON_MIN_W))
    }

    fn is_detail(&self) -> bool {
        self.screen == Screen::Detail && self.session.is_some()
    }

    /// 窓の下の知らせ。直した結果がなければ、直せないリプレイの理由。
    fn message(&self) -> Option<(String, Tone)> {
        let t = self.text();
        if let Some(n) = &self.notice {
            let tone = match n {
                Notice::Made(_) => Tone::Good,
                Notice::Unchanged | Notice::TooMany => Tone::Normal,
                Notice::NothingFound | Notice::Exists(_) | Notice::Failed(_) => Tone::Bad,
            };
            return Some((t.notice(n), tone));
        }
        let a = self.session.as_ref()?.analysis()?;
        t.unavailable(a).map(|s| (s, Tone::Normal))
    }

    // ---- 部品の中身 ----

    fn set_message(&self) {
        let (text, tone) = self.message().unwrap_or((String::new(), Tone::Normal));
        MESSAGE_STYLE.with(|m| {
            m.set(MessageStyle {
                hwnd: self.c.message as usize,
                color: tone_color(tone),
            });
        });
        set_text(self.c.message, &text);
    }

    /// 文言・値・有効／無効を、いまの言語と状態に合わせる。並べ方は `relayout`。
    fn fill(&self) {
        let (t, c) = (self.text(), &self.c);
        set_text(self.hwnd, t.title());
        set_text(c.lang_label, t.language_label());
        combo_select(c.lang_combo, Some(self.lang.index()));
        set_text(c.close, t.close());
        self.set_message();
        let Some(s) = self.session.as_ref().filter(|_| self.is_detail()) else {
            set_text(c.hint, t.start_hint());
            set_text(c.choose_replays, t.choose_replays());
            set_text(c.choose_folder, t.choose_folder());
            return;
        };
        let fixable = s.fixable();
        set_text(c.path, &format!("{}{}", t.file_label(), s.path().display()));
        set_text(c.heads[0], "1P");
        set_text(c.heads[1], "2P");
        let labels = [
            t.side_label(),
            t.name_label(),
            t.current_label(),
            t.new_value_label(),
        ];
        for (h, text) in c.row_labels.into_iter().zip(labels) {
            set_text(h, text);
        }
        let dash = "\u{2014}".to_string();
        let (side1, side2) = match fixable.and_then(|f| f.side) {
            Some(Side::P1) => (t.side_word(true), t.side_word(false)),
            Some(Side::P2) => (t.side_word(false), t.side_word(true)),
            None => (dash.as_str(), dash.as_str()),
        };
        let (name1, name2) = fixable.map_or((dash.clone(), dash.clone()), |f| f.names.clone());
        let (cur1, cur2) = fixable.map_or((dash.as_str(), dash.as_str()), |f| {
            (f.current.0.name(), f.current.1.name())
        });
        let rows = [
            (side1, side2),
            (name1.as_str(), name2.as_str()),
            (cur1, cur2),
        ];
        for (cells, (a, b)) in c.cells.into_iter().zip(rows) {
            set_text(cells[0], a);
            set_text(cells[1], b);
        }
        // 直せないリプレイでは選択欄を空にして押せなくする（理由は知らせの行に出る）。
        let pick = s.pick();
        for (i, h) in c.drops.into_iter().enumerate() {
            let kind = pick.map(|p| if i == 0 { p.0 } else { p.1 });
            combo_select(h, kind.and_then(|k| Kind::ALL.iter().position(|x| *x == k)));
            enable(h, pick.is_some());
        }
        set_text(c.prev, t.prev());
        set_text(c.next, t.next());
        set_text(c.pos, &t.position(s.index(), s.len()));
        set_text(c.make, t.make());
        set_text(c.make_all, t.make_all());
        enable(c.prev, s.can_prev());
        enable(c.next, s.can_next());
        enable(c.make, fixable.is_some());
        enable(c.make_all, fixable.is_some());
    }

    /// 部品を窓のダイアログの決まり（余白・ボタンの大きさと右寄せ・中央の「前へ」「次へ」）で並べる。
    fn relayout(&self) {
        let (t, c) = (self.text(), &self.c);
        let detail = self.is_detail();
        let s = |v: i32| self.sc(v);
        let (w, m) = (s(CLIENT_W), s(MARGIN));
        let bh = s(BUTTON_H);

        let hidden: Vec<HWND> = if detail {
            c.start_only().to_vec()
        } else {
            c.detail_only()
        };
        for h in hidden {
            hide(h);
        }

        // 右上: 言語
        let combo_w = Lang::ALL
            .iter()
            .map(|l| self.text_width(lang_name(*l)))
            .max()
            .unwrap_or(0)
            + s(40);
        let combo_x = w - m - combo_w;
        place(
            c.lang_combo,
            combo_x,
            s(10),
            combo_w,
            s(COMBO_H + COMBO_LIST_H),
        );
        let label_w = self.text_width(t.language_label()) + s(4);
        let label_x = combo_x - s(6) - label_w;
        place(c.lang_label, label_x, s(14), label_w, s(18));

        // 窓の下: 区切りの線と、右下の「終了」。直す画面の 2 つは左寄せ、最初の画面の 2 つは「終了」の左に右寄せ。
        place(c.separator, m, s(SEPARATOR_Y), w - 2 * m, s(2));
        let exit_w = self.button_width(t.close());
        let mut x = w - m - exit_w;
        place(c.close, x, s(BUTTON_Y), exit_w, bh);
        x -= s(BUTTON_GAP_EXIT);
        if detail {
            let mut lx = m;
            for (h, text) in [(c.make, t.make()), (c.make_all, t.make_all())] {
                let bw = self.button_width(text);
                place(h, lx, s(BUTTON_Y), bw, bh);
                lx += bw + s(BUTTON_GAP);
            }
        } else {
            for (h, text) in [
                (c.choose_folder, t.choose_folder()),
                (c.choose_replays, t.choose_replays()),
            ] {
                let bw = self.button_width(text);
                x -= bw;
                place(h, x, s(BUTTON_Y), bw, bh);
                x -= s(BUTTON_GAP);
            }
        }

        if !detail {
            place(c.hint, m, s(48), w - 2 * m, s(70));
            place(c.message, m, s(130), w - 2 * m, s(60));
            return;
        }

        // 左上: ファイルのパス
        place(c.path, m, s(14), (label_x - s(8) - m).max(s(40)), s(18));
        // 表: 見出し・側・名前・今の設定
        let (col1, col2, col_w) = (s(140), s(350), s(198));
        let (head_y, row_y, row_h) = (s(50), s(72), s(24));
        for (h, x) in c.heads.into_iter().zip([col1, col2]) {
            place(h, x, head_y, col_w, s(18));
        }
        for i in 0..4 {
            let y = if i < 3 {
                row_y + i as i32 * row_h
            } else {
                s(150)
            };
            place(c.row_labels[i], m, y + s(3), col1 - m - s(8), s(18));
            if i < 3 {
                for (h, x) in c.cells[i].into_iter().zip([col1, col2]) {
                    place(h, x, y + s(3), col_w, s(18));
                }
            }
        }
        for (h, x) in c.drops.into_iter().zip([col1, col2]) {
            place(h, x, s(148), col_w, s(COMBO_H + COMBO_LIST_H));
        }
        place(c.message, m, s(184), w - 2 * m, s(44));
        // 中央: ◀ 前へ / n / N / 次へ ▶
        let (pw, nw, pos_w) = (
            self.button_width(t.prev()),
            self.button_width(t.next()),
            s(90),
        );
        let x0 = (w - (pw + pos_w + nw)) / 2;
        let ny = s(236);
        place(c.prev, x0, ny, pw, bh);
        place(c.pos, x0 + pw, ny + s(4), pos_w, s(18));
        place(c.next, x0 + pw + pos_w, ny, nw, bh);
    }

    /// 画面が切り替わったら既定のボタンと最初のフォーカスを置く。そうでなくても、フォーカスのある部品が
    /// 押せなくなっていたら（最後の 1 本で「次へ」を押したなど）既定のボタンへ移す。
    fn settle_focus(&mut self) {
        let now = self.screen_now();
        let default = if now == Screen::Detail {
            Id::Make
        } else {
            Id::ChooseReplays
        };
        let changed = self.shown != Some(now);
        self.shown = Some(now);
        if changed {
            send(self.hwnd, DM_SETDEFID, default.ctl() as WPARAM, 0);
        }
        // SAFETY: フォーカスの読み取りだけ。
        let f = unsafe { GetFocus() };
        let usable = !f.is_null()
            // SAFETY: `f` は GetFocus が返した窓。親子と可視の判定だけ。
            && unsafe { IsChild(self.hwnd, f) != 0 && IsWindowVisible(f) != 0 }
            && is_enabled(f);
        if changed || !usable {
            let target = [default, Id::Close]
                .into_iter()
                .map(|id| self.control(id))
                .find(|h| is_enabled(*h))
                .unwrap_or(self.c.close);
            // SAFETY: 自分の窓の部品へのフォーカス。他のプロセスの窓や前面には触らない。
            unsafe { SetFocus(target) };
        }
    }

    fn screen_now(&self) -> Screen {
        if self.is_detail() {
            Screen::Detail
        } else {
            Screen::Start
        }
    }

    fn control(&self, id: Id) -> HWND {
        match id {
            Id::ChooseReplays => self.c.choose_replays,
            Id::ChooseFolder => self.c.choose_folder,
            Id::Close => self.c.close,
            Id::Prev => self.c.prev,
            Id::Next => self.c.next,
            Id::Make => self.c.make,
            Id::MakeAll => self.c.make_all,
            Id::Language => self.c.lang_combo,
            Id::Drop(Player::P1) => self.c.drops[0],
            Id::Drop(Player::P2) => self.c.drops[1],
        }
    }

    /// 状態を部品に反映し直す。
    fn refresh(&mut self) {
        self.fill();
        self.relayout();
        self.settle_focus();
    }

    // ---- 状態の変更 ----

    /// 選ばれたもの（ファイル・フォルダ）を読み込んで、1 本ずつの画面へ。1 本も無ければ今の画面のまま知らせる。
    fn load(&mut self, inputs: &[PathBuf]) {
        let found = collect(inputs);
        match Session::new(found.files) {
            Some(mut s) => {
                s.ensure_analyzed(analyze);
                self.session = Some(s);
                self.screen = Screen::Detail;
                self.notice = found.truncated.then_some(Notice::TooMany);
            }
            None => self.notice = Some(Notice::NothingFound),
        }
        self.refresh();
    }

    fn step(&mut self, forward: bool) {
        if let Some(s) = &mut self.session {
            if forward {
                s.next();
            } else {
                s.prev();
            }
            s.ensure_analyzed(analyze);
        }
        self.notice = None;
        self.refresh();
    }

    /// 調べたときは直せたのに、直す時点で直せないと分かったとき（ファイルが書き換わった）の知らせ。
    fn refused(&self, a: Analysis) -> Notice {
        Notice::Failed(self.text().unavailable(&a).unwrap_or_default())
    }

    /// 作れたときだけ、結果の窓に出す内容を返す。
    fn make_current(&mut self) -> Option<Report> {
        let s = self.session.as_ref()?;
        let (path, pair) = match s.make_state() {
            MakeState::Unavailable => return None,
            MakeState::Unchanged => {
                self.notice = Some(Notice::Unchanged);
                self.refresh();
                return None;
            }
            MakeState::Ready(pair) => (s.path().to_path_buf(), pair),
        };
        let result = make(&path, pair);
        self.notice = Some(match &result {
            MakeResult::Made(p) => Notice::Made(p.clone()),
            MakeResult::Unchanged => Notice::Unchanged,
            MakeResult::Exists(p) => Notice::Exists(p.clone()),
            MakeResult::Recorded => self.refused(Analysis::Recorded),
            MakeResult::NoMark => self.refused(Analysis::NoMark),
            MakeResult::OddValue(a, b) => self.refused(Analysis::OddValue(*a, *b)),
            MakeResult::Failed(e) => Notice::Failed(e.clone()),
        });
        self.refresh();
        let made = matches!(result, MakeResult::Made(_));
        made.then(|| {
            let mut r = Report::default();
            r.add(0, &path, result);
            r
        })
    }

    /// 「直した後の設定」の選択欄が変わった。選択欄そのものは作り直さず、知らせの行だけ消す。
    fn pick_changed(&mut self, who: Player) {
        let h = self.control(Id::Drop(who));
        let Some(kind) = combo_selected(h).and_then(|i| Kind::ALL.get(i).copied()) else {
            return;
        };
        if let Some(s) = &mut self.session {
            s.set_pick(who, kind);
        }
        self.notice = None;
        self.set_message();
    }

    fn language_changed(&mut self) {
        let Some(i) = combo_selected(self.c.lang_combo) else {
            return;
        };
        self.lang = Lang::from_index(i as u8);
        self.refresh();
    }
}

// ---- 窓のメッセージ ----

fn message_box(hwnd: HWND, caption: &str, text: &str, icon: u32) {
    let (t, c) = (wide_nul(text), wide_nul(caption));
    // SAFETY: 2 つの文字列とも終端 0 つきで呼びの間生きている。`hwnd` は自分の窓（null でもよい）。
    unsafe { MessageBoxW(hwnd, t.as_ptr(), c.as_ptr(), MB_OK | icon) };
}

fn show_result(h: HWND, t: Text, report: &Report) {
    message_box(h, t.title(), &t.result_report(report), MB_ICONINFORMATION);
}

fn activate(id: Id) {
    match id {
        Id::Close => {
            if let Some(h) = with_app(|a| a.hwnd) {
                // SAFETY: 自分の窓。WM_DESTROY で終了の合図を出す。
                unsafe { DestroyWindow(h) };
            }
        }
        Id::ChooseReplays => {
            let Some((h, t)) = with_app(|a| (a.hwnd, a.text())) else {
                return;
            };
            let r = dialogs::choose_replays(h, t.open_title(), t.filter_name());
            finish_pick(r.map(|v| (!v.is_empty()).then_some(v)));
        }
        Id::ChooseFolder => {
            let Some((h, t)) = with_app(|a| (a.hwnd, a.text())) else {
                return;
            };
            let r = dialogs::choose_folder(h, t.folder_title());
            finish_pick(r.map(|o| o.map(|p| vec![p])));
        }
        Id::Prev | Id::Next => {
            with_app(|a| a.step(id == Id::Next));
        }
        Id::Make => {
            // 窓は with_app の外で出す（窓の中でメッセージ処理が回るため）。
            if let Some((h, t, r)) = with_app(|a| {
                let r = a.make_current()?;
                Some((a.hwnd, a.text(), r))
            })
            .flatten()
            {
                show_result(h, t, &r);
            }
        }
        Id::MakeAll => make_whole_folder(),
        Id::Language | Id::Drop(_) => {}
    }
}

fn finish_pick(r: Result<Option<Vec<PathBuf>>, String>) {
    match r {
        Ok(Some(paths)) => {
            with_app(|a| a.load(&paths));
        }
        Ok(None) => {}
        Err(e) => {
            with_app(|a| {
                a.notice = Some(Notice::Failed(e));
                a.refresh();
            });
        }
    }
}

fn make_whole_folder() {
    let Some((h, t, pair, targets)) = with_app(|a| {
        let s = a.session.as_ref()?;
        s.fixable()?;
        Some((a.hwnd, a.text(), s.pick()?, s.folder_targets()))
    })
    .flatten() else {
        return;
    };
    // SAFETY: 標準のカーソルを読み、処理の間だけ待ちの形にして元へ戻す。
    let old = unsafe { SetCursor(LoadCursorW(ptr::null_mut(), IDC_WAIT)) };
    let report = make_all(&targets, pair);
    // SAFETY: 上で得たカーソル（null でもよい）を戻す。
    unsafe { SetCursor(old) };
    show_result(h, t, &report);
    with_app(|a| {
        a.notice = None;
        a.refresh();
    });
}

fn drop_files(hdrop: HDROP) {
    let mut paths = Vec::new();
    // SAFETY: `hdrop` は WM_DROPFILES が渡した有効なハンドル。0xFFFFFFFF で件数、バッファは「長さ + 1」を渡す。終えたら DragFinish。
    unsafe {
        let n = DragQueryFileW(hdrop, u32::MAX, ptr::null_mut(), 0);
        for i in 0..n {
            let len = DragQueryFileW(hdrop, i, ptr::null_mut(), 0) as usize;
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(hdrop, i, buf.as_mut_ptr(), buf.len() as u32) as usize;
            paths.push(PathBuf::from(
                <std::ffi::OsString as std::os::windows::ffi::OsStringExt>::from_wide(
                    &buf[..got.min(len)],
                ),
            ));
        }
        DragFinish(hdrop);
    }
    with_app(|a| a.load(&paths));
}

/// `WM_COMMAND`。ボタンの押下（Enter・Esc を `IsDialogMessageW` が変換したものを含む）と、選択欄の変更。
fn on_command(wparam: WPARAM) {
    let id = (wparam & 0xFFFF) as i32;
    let code = ((wparam >> 16) & 0xFFFF) as u32;
    match Id::from_ctl(id) {
        Some(Id::Language) if code == CBN_SELCHANGE => {
            with_app(|a| a.language_changed());
        }
        Some(Id::Drop(who)) if code == CBN_SELCHANGE => {
            with_app(|a| a.pick_changed(who));
        }
        Some(id) if code == BN_CLICKED => {
            // 押せない部品の操作は無視（Enter が既定の「直す」へ来たときも同じ条件）。
            let usable = with_app(|a| is_enabled(a.control(id))).unwrap_or(false);
            if usable {
                activate(id);
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY => {
            // SAFETY: 引数は終了コードだけ。
            unsafe { PostQuitMessage(0) };
            return 0;
        }
        WM_CLOSE => {
            // SAFETY: 自分の窓。
            unsafe { DestroyWindow(hwnd) };
            return 0;
        }
        WM_COMMAND => {
            on_command(wparam);
            return 0;
        }
        WM_DROPFILES => {
            drop_files(wparam as HDROP);
            return 0;
        }
        WM_CTLCOLORSTATIC => {
            let style = MESSAGE_STYLE.with(Cell::get);
            if style.hwnd != 0 && style.hwnd == lparam as usize {
                // SAFETY: `wparam` は STATIC が渡した描画用の DC。返すのはシステムの共有ブラシ（消さない）。
                return unsafe {
                    SetTextColor(wparam as HDC, style.color);
                    SetBkMode(wparam as HDC, TRANSPARENT as i32);
                    GetSysColorBrush(COLOR_3DFACE) as LRESULT
                };
            }
        }
        _ => {}
    }
    // SAFETY: 受け取った引数をそのままダイアログ窓の既定の処理へ渡す。
    unsafe { DefDlgProcW(hwnd, msg, wparam, lparam) }
}

/// 窓を出して、閉じるまで回る。`initial` は最初から選んだ状態にするもの。
pub fn run(lang: Lang, initial: &[PathBuf]) -> Result<(), String> {
    let class = wide_nul(CLASS_NAME);
    let title = wide_nul(Text(lang).title());
    // SAFETY: 窓クラスの登録・窓の作成・メッセージループという通常の手順。文字列は終端 0 つきで呼びの間生きている。
    // 窓の状態は thread_local に置き、窓のメッセージの再入では借りられなければ既定の処理へ流す。
    unsafe {
        SetProcessDPIAware();
        let icc = INITCOMMONCONTROLSEX {
            dwSize: core::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES,
        };
        InitCommonControlsEx(&icc);
        let hinst = GetModuleHandleW(ptr::null());
        let wc = WNDCLASSEXW {
            cbSize: core::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            // DefDlgProcW が既定のボタンなどを覚えておく場所。
            cbWndExtra: DLGWINDOWEXTRA as i32,
            hInstance: hinst,
            hIcon: ptr::null_mut(),
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_3DFACE + 1) as usize as *mut c_void,
            lpszMenuName: ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: ptr::null_mut(),
        };
        if RegisterClassExW(&wc) == 0 {
            return Err("RegisterClassExW".to_string());
        }
        let screen_dc = GetDC(ptr::null_mut());
        let dpi = GetDeviceCaps(screen_dc, LOGPIXELSX as i32).max(96);
        ReleaseDC(ptr::null_mut(), screen_dc);
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
        let mut outer = RECT {
            left: 0,
            top: 0,
            right: CLIENT_W * dpi / 96,
            bottom: CLIENT_H * dpi / 96,
        };
        AdjustWindowRectEx(&mut outer, style, 0, WS_EX_ACCEPTFILES);
        let hwnd = CreateWindowExW(
            WS_EX_ACCEPTFILES,
            class.as_ptr(),
            title.as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            outer.right - outer.left,
            outer.bottom - outer.top,
            ptr::null_mut(),
            ptr::null_mut(),
            hinst,
            ptr::null(),
        );
        if hwnd.is_null() {
            return Err("CreateWindowExW".to_string());
        }
        DragAcceptFiles(hwnd, 1);
        let font = system_font();
        let c = Controls::create(hwnd, font, hinst);
        APP.with(|cell| {
            *cell.borrow_mut() = Some(App {
                hwnd,
                lang,
                dpi,
                font,
                screen: Screen::Start,
                shown: None,
                session: None,
                notice: None,
                c,
            });
        });
        with_app(|a| {
            if initial.is_empty() {
                a.refresh();
            } else {
                a.load(initial);
            }
        });
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        // 窓が前面になった後で、既定のボタンへフォーカスを置き直す（窓を出す前の SetFocus は最初の部品に取られうる）。
        with_app(|a| {
            a.shown = None;
            a.settle_focus();
        });
        let mut msg: MSG = core::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    Ok(())
}
