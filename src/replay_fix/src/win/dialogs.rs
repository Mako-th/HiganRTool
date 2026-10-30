//! Windows標準の「ファイルを開く」（`.rpy`・複数選択）と「フォルダーの選択」（`IFileOpenDialog`）。
//!
//! `windows-sys`はCOMのインターフェースを呼び出すための型を持たない。
//! そのため使うvtableは自分で宣言する（ローダーのフォルダ選びと同じ作り）。
//! メソッドの並びはWindows SDK 10.0.26100.0の`shobjidl_core.h`のとおり。
//! 0始まりでIUnknownが0〜2、`IModalWindow::Show`が3、`IFileDialog`は4〜26、`IFileOpenDialog::GetResults`が27。
//! 間の使わないスロットは`usize`で埋める。
//! x86のCOMはstdcall。

use core::ffi::c_void;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr;

use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows_sys::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST,
    FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
};
use windows_sys::core::GUID;

type Hresult = i32;

/// `CLSID_FileOpenDialog`。
const CLSID_FILE_OPEN_DIALOG: GUID = GUID::from_u128(0xDC1C5A9C_E88A_4DDE_A5A1_60F82A20AEF7);
/// `IID_IFileOpenDialog`。
const IID_IFILE_OPEN_DIALOG: GUID = GUID::from_u128(0xD57C7288_D4AD_4768_BE02_9D969532D960);

/// 取り消しはこの値（`HRESULT_FROM_WIN32(ERROR_CANCELLED)`）で返る。
const HRESULT_CANCELLED: Hresult = 0x8007_04C7_u32 as i32;
/// このスレッドのCOMが別の方式で初期化済み（そのまま使える）。
const RPC_E_CHANGED_MODE: Hresult = 0x8001_0106_u32 as i32;

/// `COMDLG_FILTERSPEC`（`shtypes.h`）。
#[repr(C)]
struct FilterSpec {
    name: *const u16,
    spec: *const u16,
}

#[repr(C)]
struct DialogVtbl {
    _query_interface: usize,
    _add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    show: unsafe extern "system" fn(*mut c_void, *mut c_void) -> Hresult, // 3
    set_file_types: unsafe extern "system" fn(*mut c_void, u32, *const FilterSpec) -> Hresult, // 4
    _file_type_index_and_advise: [usize; 4], // 5〜8: SetFileTypeIndex / GetFileTypeIndex / Advise / Unadvise
    set_options: unsafe extern "system" fn(*mut c_void, u32) -> Hresult, // 9
    get_options: unsafe extern "system" fn(*mut c_void, *mut u32) -> Hresult, // 10
    _folders_names: [usize; 6], // 11〜16: SetDefaultFolder / SetFolder / GetFolder / GetCurrentSelection / SetFileName / GetFileName
    set_title: unsafe extern "system" fn(*mut c_void, *const u16) -> Hresult, // 17
    _labels: [usize; 2],        // 18〜19: SetOkButtonLabel / SetFileNameLabel
    get_result: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Hresult, // 20
    _after_result: [usize; 6], // 21〜26: AddPlace / SetDefaultExtension / Close / SetClientGuid / ClearClientData / SetFilter
    get_results: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Hresult, // 27
}

#[repr(C)]
struct ItemVtbl {
    _query_interface: usize,
    _add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    _bind_to_handler_and_get_parent: [usize; 2], // 3〜4
    get_display_name: unsafe extern "system" fn(*mut c_void, i32, *mut *mut u16) -> Hresult, // 5
}

/// `IShellItemArray`。
#[repr(C)]
struct ItemArrayVtbl {
    _query_interface: usize,
    _add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    _bind_to_attributes: [usize; 4], // 3〜6: BindToHandler / GetPropertyStore / GetPropertyDescriptionList / GetAttributes
    get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> Hresult, // 7
    get_item_at: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> Hresult, // 8
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
impl HasRelease for ItemArrayVtbl {
    fn release_fn(&self) -> unsafe extern "system" fn(*mut c_void) -> u32 {
        self.release
    }
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
        // SAFETY: `from_raw`の約束で先頭の1ワードがvtableへのポインタ。`self`が持つ間はvtableも生きている。
        unsafe { &**(self.raw as *const *const V) }
    }
}

impl<V: HasRelease> Drop for Com<V> {
    fn drop(&mut self) {
        let release = self.vtbl().release_fn();
        // SAFETY: `self.raw`はまだ生きている（Dropは1回だけ）。受け取った参照を1つ返す。
        unsafe { release(self.raw) };
    }
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
            Err(hr_text("CoInitializeEx", hr))
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

fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 出さずに用意するところまで（検査がウィンドウを出さずにvtableの並びをチェックするため）。
fn make_dialog(
    title: &str,
    extra_options: u32,
    filter: Option<(&str, &str)>,
) -> Result<Com<DialogVtbl>, String> {
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
    opts |= FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | extra_options;
    // SAFETY: 同上。
    let hr = unsafe { (dlg.vtbl().set_options)(dlg.raw, opts) };
    if hr < 0 {
        return Err(hr_text("SetOptions", hr));
    }
    let title_w = wide_nul(title);
    // SAFETY: `title_w`は終端0つきで呼びの間生きている。
    let hr = unsafe { (dlg.vtbl().set_title)(dlg.raw, title_w.as_ptr()) };
    if hr < 0 {
        return Err(hr_text("SetTitle", hr));
    }
    if let Some((name, spec)) = filter {
        let (name_w, spec_w) = (wide_nul(name), wide_nul(spec));
        let specs = [FilterSpec {
            name: name_w.as_ptr(),
            spec: spec_w.as_ptr(),
        }];
        // SAFETY: `specs`は1件。2つの文字列とも終端0つき。SetFileTypesは呼びの中で内容を複製する。
        let hr = unsafe { (dlg.vtbl().set_file_types)(dlg.raw, 1, specs.as_ptr()) };
        if hr < 0 {
            return Err(hr_text("SetFileTypes", hr));
        }
    }
    Ok(dlg)
}

/// 項目1つのファイルシステム上のパス。
fn item_path(item: &Com<ItemVtbl>) -> Result<PathBuf, String> {
    let mut name: *mut u16 = ptr::null_mut();
    // SAFETY: `item`は生きている。`name`は書き込み可能な1ワード。CoTaskMemFreeで解放する文字列を指す。
    let hr = unsafe { (item.vtbl().get_display_name)(item.raw, SIGDN_FILESYSPATH, &mut name) };
    if hr < 0 || name.is_null() {
        return Err(hr_text("GetDisplayName", hr));
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
    Ok(path)
}

/// 出して閉じるまで待つ。取り消しは`Ok(false)`。
fn show(dlg: &Com<DialogVtbl>, owner: *mut c_void) -> Result<bool, String> {
    // SAFETY: `dlg`は生きている。`owner`は呼び出し側のウィンドウ（nullでもよい）。Showは人が閉じるまで戻らない。
    let hr = unsafe { (dlg.vtbl().show)(dlg.raw, owner) };
    if hr == HRESULT_CANCELLED {
        return Ok(false);
    }
    if hr < 0 {
        return Err(hr_text("Show", hr));
    }
    Ok(true)
}

/// 「フォルダーの選択」。取り消しは`Ok(None)`。
pub fn choose_folder(owner: *mut c_void, title: &str) -> Result<Option<PathBuf>, String> {
    let _scope = ComScope::enter()?;
    let dlg = make_dialog(title, FOS_PICKFOLDERS, None)?;
    if !show(&dlg, owner)? {
        return Ok(None);
    }
    let mut item_raw: *mut c_void = ptr::null_mut();
    // SAFETY: `dlg`は生きている。`item_raw`は書き込み可能な1ワード。
    let hr = unsafe { (dlg.vtbl().get_result)(dlg.raw, &mut item_raw) };
    if hr < 0 || item_raw.is_null() {
        return Err(hr_text("GetResult", hr));
    }
    // SAFETY: 上で得たIShellItemの参照1つ。
    let item: Com<ItemVtbl> = unsafe { Com::from_raw(item_raw) };
    item_path(&item).map(Some)
}

/// 「ファイルを開く」（`.rpy`・複数選択）。取り消しは空。
pub fn choose_replays(
    owner: *mut c_void,
    title: &str,
    filter_name: &str,
) -> Result<Vec<PathBuf>, String> {
    let _scope = ComScope::enter()?;
    let dlg = make_dialog(
        title,
        FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST,
        Some((filter_name, "*.rpy")),
    )?;
    if !show(&dlg, owner)? {
        return Ok(Vec::new());
    }
    let mut arr_raw: *mut c_void = ptr::null_mut();
    // SAFETY: `dlg`は生きている。`arr_raw`は書き込み可能な1ワード。
    let hr = unsafe { (dlg.vtbl().get_results)(dlg.raw, &mut arr_raw) };
    if hr < 0 || arr_raw.is_null() {
        return Err(hr_text("GetResults", hr));
    }
    // SAFETY: 上で得たIShellItemArrayの参照1つ。
    let arr: Com<ItemArrayVtbl> = unsafe { Com::from_raw(arr_raw) };
    let mut count = 0u32;
    // SAFETY: `arr`は生きている。`count`は書き込み可能なu32。
    let hr = unsafe { (arr.vtbl().get_count)(arr.raw, &mut count) };
    if hr < 0 {
        return Err(hr_text("GetCount", hr));
    }
    let mut out = Vec::new();
    for i in 0..count {
        let mut item_raw: *mut c_void = ptr::null_mut();
        // SAFETY: `arr`は生きている。`i`は`count`の範囲内。`item_raw`は書き込み可能な1ワード。
        let hr = unsafe { (arr.vtbl().get_item_at)(arr.raw, i, &mut item_raw) };
        if hr < 0 || item_raw.is_null() {
            return Err(hr_text("GetItemAt", hr));
        }
        // SAFETY: 上で得たIShellItemの参照1つ。
        let item: Com<ItemVtbl> = unsafe { Com::from_raw(item_raw) };
        out.push(item_path(&item)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vtable_declarations_have_the_sdk_slot_count() {
        let word = core::mem::size_of::<usize>();
        assert_eq!(core::mem::size_of::<DialogVtbl>(), 28 * word);
        assert_eq!(core::mem::size_of::<ItemVtbl>(), 6 * word);
        assert_eq!(core::mem::size_of::<ItemArrayVtbl>(), 9 * word);
    }

    #[test]
    fn the_dialogs_are_set_up_for_their_purpose_without_being_shown() {
        let _scope = ComScope::enter().expect("COM が初期化できない");
        let opts_of = |d: &Com<DialogVtbl>| {
            let mut o = 0u32;
            // SAFETY: `d`は生きている。`o`は書き込み可能なu32。
            let hr = unsafe { (d.vtbl().get_options)(d.raw, &mut o) };
            assert!(hr >= 0);
            o
        };
        let folder = make_dialog("t", FOS_PICKFOLDERS, None).expect("フォルダの窓の用意");
        assert_eq!(opts_of(&folder) & FOS_PICKFOLDERS, FOS_PICKFOLDERS);
        let files = make_dialog(
            "t",
            FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST,
            Some(("Replays", "*.rpy")),
        )
        .expect("ファイルの窓の用意");
        let o = opts_of(&files);
        assert_eq!(
            o & FOS_ALLOWMULTISELECT,
            FOS_ALLOWMULTISELECT,
            "複数選択でない"
        );
        assert_eq!(o & FOS_PICKFOLDERS, 0, "フォルダ選びになっている");
    }
}
