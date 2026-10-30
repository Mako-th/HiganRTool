//! Windows標準のフォルダ選択のウィンドウ（`IFileOpenDialog`に`FOS_PICKFOLDERS`）とメッセージボックス。`game_pick::Host`の本物。
//!
//! `windows-sys`はCOMのインターフェースの呼び出しの型を持たないので使うvtableを自分で宣言する。
//! 並びはWindows SDK 10.0.26100.0の`shobjidl_core.h`。
//! 0始まりでIUnknownが0〜2、`IModalWindow::Show`が3、`IFileDialog`は4から。
//! 間の使わない行は`usize`で埋める。x86のCOMはstdcall。親のウィンドウは無し（前面を奪わない）。

use core::ffi::c_void;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows_sys::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, SHCreateItemFromParsingName,
    SIGDN_FILESYSPATH,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_DEFBUTTON1, MB_ICONQUESTION, MB_ICONWARNING, MB_OK, MB_YESNO, MessageBoxW,
};
use windows_sys::core::GUID;

use crate::game_pick::Answer;

type Hresult = i32;

/// `CLSID_FileOpenDialog`（`shobjidl_core.h`）。
const CLSID_FILE_OPEN_DIALOG: GUID = GUID::from_u128(0xDC1C5A9C_E88A_4DDE_A5A1_60F82A20AEF7);
/// `IID_IFileOpenDialog`。
const IID_IFILE_OPEN_DIALOG: GUID = GUID::from_u128(0xD57C7288_D4AD_4768_BE02_9D969532D960);
/// `IID_IShellItem`。
const IID_ISHELL_ITEM: GUID = GUID::from_u128(0x43826D1E_E718_42EE_BC55_A1E261C37BFE);

/// ウィンドウでキャンセルするとこの値（`HRESULT_FROM_WIN32(ERROR_CANCELLED)`）が返る。
const HRESULT_CANCELLED: Hresult = 0x8007_04C7_u32 as i32;
/// このスレッドのCOMが別の方式で初期化済み（そのまま使える）。
const RPC_E_CHANGED_MODE: Hresult = 0x8001_0106_u32 as i32;

#[repr(C)]
struct DialogVtbl {
    _query_interface: usize,
    _add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    show: unsafe extern "system" fn(*mut c_void, *mut c_void) -> Hresult, // 3
    _file_types: [usize; 5], // 4〜8: SetFileTypes / SetFileTypeIndex / GetFileTypeIndex / Advise / Unadvise
    set_options: unsafe extern "system" fn(*mut c_void, u32) -> Hresult, // 9
    get_options: unsafe extern "system" fn(*mut c_void, *mut u32) -> Hresult, // 10
    _set_default_folder: usize, // 11
    set_folder: unsafe extern "system" fn(*mut c_void, *mut c_void) -> Hresult, // 12
    _get_folder_to_set_file_name: [usize; 4], // 13〜16: GetFolder / GetCurrentSelection / SetFileName / GetFileName
    set_title: unsafe extern "system" fn(*mut c_void, *const u16) -> Hresult, // 17
    _ok_button_and_file_name_label: [usize; 2], // 18〜19: SetOkButtonLabel / SetFileNameLabel
    get_result: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Hresult, // 20
}

#[repr(C)]
struct ItemVtbl {
    _query_interface: usize,
    _add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    _bind_to_handler_and_get_parent: [usize; 2], // 3〜4
    get_display_name: unsafe extern "system" fn(*mut c_void, i32, *mut *mut u16) -> Hresult, // 5
}

struct Com<V: HasRelease> {
    raw: *mut c_void,
    _vtbl: core::marker::PhantomData<V>,
}

impl<V: HasRelease> Com<V> {
    /// # Safety
    /// `raw`は`V`の並びのvtableを持つ、参照を1つ渡されたCOMオブジェクトであること。
    unsafe fn from_raw(raw: *mut c_void) -> Self {
        Com {
            raw,
            _vtbl: core::marker::PhantomData,
        }
    }

    fn vtbl(&self) -> &V {
        // SAFETY: `from_raw`の約束で先頭の1ワードがvtableへのポインタ。`self`が持つ間vtableも生きている。
        unsafe { &**(self.raw as *const *const V) }
    }
}

trait HasRelease {
    fn release_fn(&self) -> unsafe extern "system" fn(*mut c_void) -> u32;
}
impl HasRelease for DialogVtbl {
    fn release_fn(&self) -> unsafe extern "system" fn(*mut c_void) -> u32 {
        self.release
    }
}
impl HasRelease for ItemVtbl {
    fn release_fn(&self) -> unsafe extern "system" fn(*mut c_void) -> u32 {
        self.release
    }
}

impl<V: HasRelease> Drop for Com<V> {
    fn drop(&mut self) {
        let release = self.vtbl().release_fn();
        // SAFETY: `self.raw`はまだ生きていて（Dropは1回だけ）、受け取った参照を1つ返す。
        unsafe { release(self.raw) };
    }
}

fn wide_nul(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

struct ComScope {
    uninit: bool,
}

impl ComScope {
    fn enter() -> Result<Self, String> {
        // SAFETY: 引数は予約（null）と定数だけ。
        let hr = unsafe {
            CoInitializeEx(
                ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        if hr >= 0 {
            Ok(ComScope { uninit: true })
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(ComScope { uninit: false })
        } else {
            Err(format!("CoInitializeEx、HRESULT 0x{:08X}", hr as u32))
        }
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.uninit {
            // SAFETY: `enter`で成功したCoInitializeExと対（同じスレッドで1回）。
            unsafe { CoUninitialize() };
        }
    }
}

fn hr_text(what: &str, hr: Hresult) -> String {
    format!("{what}、HRESULT 0x{:08X}", hr as u32)
}

/// ウィンドウの用意まで。出さない（検査が出さずにvtableの並びをチェックするため）。
fn make_dialog(title: &str, initial: Option<&Path>) -> Result<Com<DialogVtbl>, String> {
    let mut raw: *mut c_void = ptr::null_mut();
    // SAFETY: CLSIDとIIDは定数。`raw`は書き込み可能な1ワード。成功すれば参照を1つ得る。
    let hr = unsafe {
        CoCreateInstance(
            &CLSID_FILE_OPEN_DIALOG,
            ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_IFILE_OPEN_DIALOG,
            &mut raw,
        )
    };
    if hr < 0 || raw.is_null() {
        return Err(hr_text("CoCreateInstance(FileOpenDialog)", hr));
    }
    // SAFETY: IFileOpenDialogはIFileDialogのvtableを前方に含む（`shobjidl_core.h`）。参照は上で1つ得た。
    let dlg: Com<DialogVtbl> = unsafe { Com::from_raw(raw) };

    let mut opts: u32 = 0;
    // SAFETY: `dlg`は生きている。`opts`は書き込み可能なu32。
    let hr = unsafe { (dlg.vtbl().get_options)(dlg.raw, &mut opts) };
    if hr < 0 {
        return Err(hr_text("GetOptions", hr));
    }
    opts |= FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST;
    // SAFETY: 同上。
    let hr = unsafe { (dlg.vtbl().set_options)(dlg.raw, opts) };
    if hr < 0 {
        return Err(hr_text("SetOptions", hr));
    }
    let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `title_w`は終端0つきで呼び出しの間生きている。
    let hr = unsafe { (dlg.vtbl().set_title)(dlg.raw, title_w.as_ptr()) };
    if hr < 0 {
        return Err(hr_text("SetTitle", hr));
    }
    if let Some(dir) = initial {
        let dir_w = wide_nul(dir.as_os_str());
        let mut item: *mut c_void = ptr::null_mut();
        // SAFETY: `dir_w`は終端0つき。`item`は書き込み可能な1ワード。成功すればIShellItemの参照を1つ得る。
        let hr = unsafe {
            SHCreateItemFromParsingName(
                dir_w.as_ptr(),
                ptr::null_mut(),
                &IID_ISHELL_ITEM,
                &mut item,
            )
        };
        // 初めの場所が使えなくてもウィンドウは出せる（ウィンドウの既定の場所で開く）ので失敗は捨てる。
        if hr >= 0 && !item.is_null() {
            // SAFETY: 上で得たIShellItemの参照1つ。`Com`がReleaseする。
            let item: Com<ItemVtbl> = unsafe { Com::from_raw(item) };
            // SAFETY: `dlg` / `item`とも生きている。SetFolderは自分で参照を持つ。
            let _ = unsafe { (dlg.vtbl().set_folder)(dlg.raw, item.raw) };
        }
    }
    Ok(dlg)
}

pub fn choose_folder(title: &str, initial: Option<&Path>) -> Answer {
    let _scope = match ComScope::enter() {
        Ok(s) => s,
        Err(e) => return Answer::Failed(e),
    };
    let dlg = match make_dialog(title, initial) {
        Ok(d) => d,
        Err(e) => return Answer::Failed(e),
    };
    // SAFETY: 親のウィンドウは無し（null）。`dlg`は生きている。Showは人が閉じるまで戻らない。
    let hr = unsafe { (dlg.vtbl().show)(dlg.raw, ptr::null_mut()) };
    if hr == HRESULT_CANCELLED {
        return Answer::Cancelled;
    }
    if hr < 0 {
        return Answer::Failed(hr_text("Show", hr));
    }
    let mut item_raw: *mut c_void = ptr::null_mut();
    // SAFETY: `dlg`は生きている。`item_raw`は書き込み可能な1ワード。
    let hr = unsafe { (dlg.vtbl().get_result)(dlg.raw, &mut item_raw) };
    if hr < 0 || item_raw.is_null() {
        return Answer::Failed(hr_text("GetResult", hr));
    }
    // SAFETY: 上で得たIShellItemの参照1つ。
    let item: Com<ItemVtbl> = unsafe { Com::from_raw(item_raw) };
    let mut name: *mut u16 = ptr::null_mut();
    // SAFETY: `item`は生きている。`name`は書き込み可能な1ワードで、CoTaskMemFreeで返す文字列を指す。
    let hr = unsafe { (item.vtbl().get_display_name)(item.raw, SIGDN_FILESYSPATH, &mut name) };
    if hr < 0 || name.is_null() {
        return Answer::Failed(hr_text("GetDisplayName", hr));
    }
    // SAFETY: `name`は終端0のUTF-16を指す。長さを数えてコピーしてから返す。
    let path = unsafe {
        let mut len = 0usize;
        while *name.add(len) != 0 {
            len += 1;
        }
        let s = OsString::from_wide(core::slice::from_raw_parts(name, len));
        CoTaskMemFree(name as *const c_void);
        PathBuf::from(s)
    };
    Answer::Folder(path)
}

/// 説明のウィンドウ。黒いウィンドウにも出すので失敗は捨てる。
pub fn message_box(text: &str) {
    let text_w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let caption: Vec<u16> = "HiganRTool"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: 2つの文字列とも終端0つきで呼び出しの間生きている。親のウィンドウは無し（null）。
    unsafe {
        MessageBoxW(
            ptr::null_mut(),
            text_w.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONWARNING,
        )
    };
}

/// 「はい」だけがtrue（閉じる・Escは「いいえ」）。ウィンドウが出せなかったときもfalse: 利用者の許しなしに更新しない。
pub fn ask_yes_no(text: &str) -> bool {
    let text_w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let caption: Vec<u16> = "HiganRTool"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: 2つの文字列とも終端0つきで呼び出しの間生きている。親のウィンドウは無し（null）。
    let r = unsafe {
        MessageBoxW(
            ptr::null_mut(),
            text_w.as_ptr(),
            caption.as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON1,
        )
    };
    r == IDYES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dialog_is_set_up_for_folders_without_being_shown() {
        let _scope = ComScope::enter().expect("COM が初期化できない");
        let dir = std::env::temp_dir();
        let dlg =
            make_dialog(crate::game_pick::DIALOG_TITLE.ja, Some(&dir)).expect("ダイアログの用意");
        let mut opts = 0u32;
        // SAFETY: `dlg`は生きている。`opts`は書き込み可能なu32。
        let hr = unsafe { (dlg.vtbl().get_options)(dlg.raw, &mut opts) };
        assert!(hr >= 0);
        assert_eq!(
            opts & FOS_PICKFOLDERS,
            FOS_PICKFOLDERS,
            "フォルダ選びになっていない"
        );
        assert_eq!(opts & FOS_FORCEFILESYSTEM, FOS_FORCEFILESYSTEM);
    }

    #[test]
    fn vtable_declarations_have_the_sdk_slot_count() {
        let word = core::mem::size_of::<usize>();
        assert_eq!(core::mem::size_of::<DialogVtbl>(), 21 * word);
        assert_eq!(core::mem::size_of::<ItemVtbl>(), 6 * word);
    }
}
