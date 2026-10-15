//! Windows 標準の「ファイルを開く」（`.rpy`・複数選択）と「フォルダーの選択」（`IFileOpenDialog`）。
//!
//! `windows-sys` は COM のインターフェースの呼び出しの型を持たないので、使う vtable を自分で宣言する（ローダーの
//! フォルダ選びと同じ作り）。並びは Windows SDK 10.0.26100.0 の `shobjidl_core.h`。0 始まりで IUnknown が 0〜2、
//! `IModalWindow::Show` が 3、`IFileDialog` は 4〜26、`IFileOpenDialog::GetResults` が 27。間の使わない行は `usize` で埋める。
//! x86 の COM は stdcall。

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
/// このスレッドの COM が別の方式で初期化済み（そのまま使える）。
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
    /// `raw` は `V` の並びの vtable を持つ、参照を 1 つ渡された COM オブジェクトであること。
    unsafe fn from_raw(raw: *mut c_void) -> Self {
        Com {
            raw,
            _vtbl: core::marker::PhantomData,
        }
    }

    fn vtbl(&self) -> &V {
        // SAFETY: `from_raw` の約束で、先頭の 1 語が vtable へのポインタ。`self` が持つ間 vtable も生きている。
        unsafe { &**(self.raw as *const *const V) }
    }
}

impl<V: HasRelease> Drop for Com<V> {
    fn drop(&mut self) {
        let release = self.vtbl().release_fn();
        // SAFETY: `self.raw` はまだ生きていて（Drop は 1 回だけ）、受け取った参照を 1 つ返す。
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
            // SAFETY: `enter` で成功した CoInitializeEx と対（同じスレッドで 1 回）。
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

/// 出さずに用意するところまで（検査が窓を出さずに vtable の並びを確かめるため）。
fn make_dialog(
    title: &str,
    extra_options: u32,
    filter: Option<(&str, &str)>,
) -> Result<Com<DialogVtbl>, String> {
    let mut raw: *mut c_void = ptr::null_mut();
    // SAFETY: CLSID と IID は定数、`raw` は書ける 1 語。成功すれば参照を 1 つ得る。
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
    // SAFETY: IFileOpenDialog は IFileDialog の vtable を前方に含む（`shobjidl_core.h`）。参照は上で 1 つ得た。
    let dlg: Com<DialogVtbl> = unsafe { Com::from_raw(raw) };

    let mut opts: u32 = 0;
    // SAFETY: `dlg` は生きている。`opts` は書ける u32。
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
    // SAFETY: `title_w` は終端 0 つきで、呼びの間生きている。
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
        // SAFETY: `specs` は 1 件で、2 つの文字列とも終端 0 つき。SetFileTypes は呼びの中で内容を複製する。
        let hr = unsafe { (dlg.vtbl().set_file_types)(dlg.raw, 1, specs.as_ptr()) };
        if hr < 0 {
            return Err(hr_text("SetFileTypes", hr));
        }
    }
    Ok(dlg)
}

/// 項目 1 つのファイルシステム上のパス。
fn item_path(item: &Com<ItemVtbl>) -> Result<PathBuf, String> {
    let mut name: *mut u16 = ptr::null_mut();
    // SAFETY: `item` は生きている。`name` は書ける 1 語で、CoTaskMemFree で返す文字列を指す。
    let hr = unsafe { (item.vtbl().get_display_name)(item.raw, SIGDN_FILESYSPATH, &mut name) };
    if hr < 0 || name.is_null() {
        return Err(hr_text("GetDisplayName", hr));
    }
    // SAFETY: `name` は終端 0 の UTF-16 を指す。長さを数えてコピーしてから返す。
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

/// 出して、閉じるまで待つ。取り消しは `Ok(false)`。
fn show(dlg: &Com<DialogVtbl>, owner: *mut c_void) -> Result<bool, String> {
    // SAFETY: `dlg` は生きている。`owner` は呼び出し側の窓（null でもよい）。Show は人が閉じるまで戻らない。
    let hr = unsafe { (dlg.vtbl().show)(dlg.raw, owner) };
    if hr == HRESULT_CANCELLED {
        return Ok(false);
    }
    if hr < 0 {
        return Err(hr_text("Show", hr));
    }
    Ok(true)
}

/// 「フォルダーの選択」。取り消しは `Ok(None)`。
pub fn choose_folder(owner: *mut c_void, title: &str) -> Result<Option<PathBuf>, String> {
    let _scope = ComScope::enter()?;
    let dlg = make_dialog(title, FOS_PICKFOLDERS, None)?;
    if !show(&dlg, owner)? {
        return Ok(None);
    }
    let mut item_raw: *mut c_void = ptr::null_mut();
    // SAFETY: `dlg` は生きている。`item_raw` は書ける 1 語。
    let hr = unsafe { (dlg.vtbl().get_result)(dlg.raw, &mut item_raw) };
    if hr < 0 || item_raw.is_null() {
        return Err(hr_text("GetResult", hr));
    }
    // SAFETY: 上で得た IShellItem の参照 1 つ。
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
    // SAFETY: `dlg` は生きている。`arr_raw` は書ける 1 語。
    let hr = unsafe { (dlg.vtbl().get_results)(dlg.raw, &mut arr_raw) };
    if hr < 0 || arr_raw.is_null() {
        return Err(hr_text("GetResults", hr));
    }
    // SAFETY: 上で得た IShellItemArray の参照 1 つ。
    let arr: Com<ItemArrayVtbl> = unsafe { Com::from_raw(arr_raw) };
    let mut count = 0u32;
    // SAFETY: `arr` は生きている。`count` は書ける u32。
    let hr = unsafe { (arr.vtbl().get_count)(arr.raw, &mut count) };
    if hr < 0 {
        return Err(hr_text("GetCount", hr));
    }
    let mut out = Vec::new();
    for i in 0..count {
        let mut item_raw: *mut c_void = ptr::null_mut();
        // SAFETY: `arr` は生きている。`i` は `count` の範囲内。`item_raw` は書ける 1 語。
        let hr = unsafe { (arr.vtbl().get_item_at)(arr.raw, i, &mut item_raw) };
        if hr < 0 || item_raw.is_null() {
            return Err(hr_text("GetItemAt", hr));
        }
        // SAFETY: 上で得た IShellItem の参照 1 つ。
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
            // SAFETY: `d` は生きている。`o` は書ける u32。
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
