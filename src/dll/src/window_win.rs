//! ウィンドウモードのウィンドウを作る呼び出し（`CreateWindowExA`。`0x42D5C2`）の差し替え。ini の `[window]` でウィンドウの大きさ・位置・タイトルバー・最前面を
//! 決める。作り方は「ウィンドウだけ大きくする」: D3D8 の裏の画面は 640×480 のまま、Present の引き伸ばしに任せる。アドレスと期待バイト列は
//! `th09_addr.rs` の「ウィンドウを作る呼び出し」の節、読みと計算は `higanr_core::window_size`（pure）。
//!
//! **時期**: 差し替えは DllMain の中（`win::on_process_attach`）で書く。ローダーは `CREATE_SUSPENDED` で作ったゲームに `LoadLibraryW` を
//! 呼ばせてから `ResumeThread` するので、ゲームのメインスレッドはウィンドウを作る地点まで一歩も走っていない（読み取りのスレッドが置く他のフックは
//! ウィンドウに間に合う保証が無いので、ここだけ別にした）。DllMain はローダーロックの下なので **ini はここで読まない**: 差し替えた関数が最初に
//! 呼ばれたとき（ウィンドウを作る前の 1 回）に読む。マッチ中に走るコードは無い。
//!
//! 書く前に、地点の前後 49 バイトを実バイトで照合し、1 バイトでも違えば何も書かない。ini が無い・`[window]` が無い・読めない値のときは、
//! 差し替えた関数は元の `CreateWindowExA` へ引数をそのまま渡すだけ。ログの行は `main_log::push` で頼む（別のファイルは作らず、ファイルを
//! 開かない・書かないので DllMain の中でもゲームのメインスレッドでも待たない）。行は設置 1・設定 1・ウィンドウを作った結果 1。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "window_win.rs は 32 ビット（i686）専用（ゲームのアドレスと stdcall の 12 引数を前提にしている）"
);

use core::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use higanr_core::patch;
use higanr_core::window_size::{self, CreateArgs, FrameMetrics, WindowConfig};
use windows_sys::Win32::Foundation::{HMODULE, HWND, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExA, GWL_STYLE, GetClientRect, GetSystemMetrics, GetWindowLongA, GetWindowRect,
    HWND_TOPMOST, SM_CXFIXEDFRAME, SM_CYCAPTION, SM_CYFIXEDFRAME, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
};

use crate::hook::HookEnv;
use crate::hook_win::RealHook;
use crate::script_win::RealScriptHost;
use crate::session::ScriptHost;
use crate::sites::SiteDef;
use crate::th09_addr::{WINDOW_CREATE_CALL_OFFSET, WINDOW_CREATE_IAT, WINDOW_CREATE_SITES};
use crate::win::module_path;
#[cfg(not(test))]
use crate::win::{module_of_addr, read_own_into};

type CreateWindowFn = unsafe extern "system" fn(
    u32,
    *const u8,
    *const u8,
    u32,
    i32,
    i32,
    i32,
    i32,
    *mut c_void,
    *mut c_void,
    *mut c_void,
    *const c_void,
) -> *mut c_void;

/// 自分の DLL のハンドル（DllMain が置く。ini の置き場 = DLL のフォルダを引くため）。
static MODULE: AtomicUsize = AtomicUsize::new(0);

/// ini から読んだ設定（初めて呼ばれたときに 1 回だけ読む）。`None` は「ウィンドウは今のまま」。
static CONFIG: OnceLock<Option<WindowConfig>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_CONFIG: std::cell::Cell<Option<Option<WindowConfig>>> =
        const { std::cell::Cell::new(None) };
    static TEST_ORIG: std::cell::Cell<Option<CreateWindowFn>> = const { std::cell::Cell::new(None) };
    static TEST_SKIP_AFTER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 検査だけ: この検査のスレッドが頼んだ行（本ログの待ち行列は全検査で 1 つなので、行の中身はこちらで見る）。
    static TEST_LINES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    /// 検査だけ: 本物の待ち行列にも頼むか（行の数に上限があるので、本物へ届くことを確かめる 1 本だけが立てる）。
    static TEST_TO_QUEUE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 本ログへ 1 行頼む（先頭に `ウィンドウ: ` を付ける）。失敗と異常の行。
fn log(line: impl Into<String>) {
    emit(line.into(), false);
}

/// 詳細のときだけ書く行（うまくいった設置・設定の読み）。
fn log_detail(line: impl Into<String>) {
    emit(line.into(), true);
}

fn emit(line: String, detail_only: bool) {
    let line = format!("ウィンドウ: {line}");
    #[cfg(test)]
    {
        TEST_LINES.with(|l| l.borrow_mut().push(line.clone()));
        if !TEST_TO_QUEUE.with(std::cell::Cell::get) {
            return;
        }
    }
    if detail_only {
        crate::main_log::push_detail(line);
    } else {
        crate::main_log::push(line);
    }
}

/// ウィンドウを作る呼び出しを自前の関数へ向ける。**DllMain から呼ぶ**（ローダーロックの下で、ミューテックスを取った後）。
/// 失敗してもロードは止めない（ウィンドウが今のままなだけ）。
pub fn install(module: HMODULE) {
    MODULE.store(module as usize, Ordering::Relaxed);
    let site = WINDOW_CREATE_SITES[0];
    let r = catch_unwind(AssertUnwindSafe(|| {
        install_at(
            &mut RealHook::new(),
            &site,
            WINDOW_CREATE_CALL_OFFSET,
            entry_address(),
        )
    }));
    let ok = matches!(r, Ok(Ok(_)));
    let line = install_line(r);
    if ok {
        log_detail(line);
    } else {
        log(line);
    }
}

/// 設置の結果の 1 行（本ログに出る）。
fn install_line(r: std::thread::Result<Result<String, String>>) -> String {
    let summary = match r {
        Ok(Ok(s)) => s,
        Ok(Err(why)) => format!("差し替えない（ウィンドウは今のまま）: {why}"),
        Err(_) => {
            "パニックを受けた（ウィンドウは今のまま。書いたかどうかは分からない）".to_string()
        }
    };
    format!("設置: {summary}")
}

fn entry_address() -> u32 {
    create_window_entry as *const () as usize as u32
}

/// `site` の範囲を読んで期待バイト列と照合し、全部合ったときだけ、`site.addr + call_off` の 6 バイト（`FF 15 abs32`）を
/// `E8 rel32 90`（`target` への call）に替える。書いた後に読み直して確かめる。書かなかったときは理由を `Err` で返す。
fn install_at(
    env: &mut dyn HookEnv,
    site: &SiteDef,
    call_off: usize,
    target: u32,
) -> Result<String, String> {
    let mut seen = vec![0u8; site.expected.len()];
    if !env.read_code(site.addr, &mut seen) {
        return Err(format!(
            "{:#010X} から {} バイトを読めない（th09.exe が無いプロセス？）",
            site.addr,
            seen.len()
        ));
    }
    patch::verify(&seen, site.expected).map_err(|e| {
        format!(
            "{:#010X} の命令が期待と違う（{e:?}）。別のバージョンの th09.exe か、vpatch の [Window] enabled がこの地点を書き換えている",
            site.addr
        )
    })?;
    let at = site.addr + call_off as u32;
    let new = window_size::call_patch(at, target);
    let done = env
        .write_code(at, &new)
        .map_err(|code| format!("{at:#010X} へ書けない（GetLastError = {code}）"))?;
    let mut back = [0u8; 6];
    if !env.read_code(at, &mut back) || back != new {
        return Err(format!(
            "{at:#010X} へ書いた後の読み直しが合わない: {back:02X?}"
        ));
    }
    Ok(format!(
        "{at:#010X} の call *{WINDOW_CREATE_IAT:#010X} を {target:#010X} への call に差し替えた（保護を戻せた: {}、命令キャッシュを流せた: {}）",
        done.protect_restored, done.flushed
    ))
}

/// `GetSystemMetrics` で、ゲームが外寸を作る式（`0x42D584` / `0x42D59E`）と同じ 3 つを引く。
fn frame_metrics() -> FrameMetrics {
    // SAFETY: 引数は定数の索引で、ポインタを渡さない。
    unsafe {
        FrameMetrics {
            fixed_frame_x: GetSystemMetrics(SM_CXFIXEDFRAME),
            fixed_frame_y: GetSystemMetrics(SM_CYFIXEDFRAME),
            caption_y: GetSystemMetrics(SM_CYCAPTION),
        }
    }
}

/// ini の `[window]`。初めて呼ばれたときに 1 回だけ読む（ウィンドウを作る前のゲームのメインスレッド。マッチ中は呼ばれない）。
fn config() -> Option<WindowConfig> {
    if let Some(c) = test_config() {
        return c;
    }
    *CONFIG.get_or_init(load_config)
}

/// 検査だけが置く設定（製品のビルドでは常に `None` で、呼びは消える）。
#[cfg(test)]
fn test_config() -> Option<Option<WindowConfig>> {
    TEST_CONFIG.with(|c| c.get())
}
#[cfg(not(test))]
fn test_config() -> Option<Option<WindowConfig>> {
    None
}

fn load_config() -> Option<WindowConfig> {
    let module = MODULE.load(Ordering::Relaxed) as HMODULE;
    let parent = |p: Option<PathBuf>| p.as_deref().and_then(Path::parent).map(Path::to_path_buf);
    let mut host = RealScriptHost {
        dll_dir: parent(module_path(module)),
        run_path: None,
        exe_dir: parent(module_path(core::ptr::null_mut())),
    };
    let origin = host.ini_origin();
    let text = match host.read_net_ini() {
        Ok(Some(t)) => t,
        Ok(None) => {
            log_detail(format!("ini が無い（{origin}）。ウィンドウは今のまま"));
            return None;
        }
        Err(why) => {
            log(format!("ini を読めない（{why}）。ウィンドウは今のまま"));
            return None;
        }
    };
    let ini = match higanr_core::ini::parse(&text) {
        Ok(i) => i,
        Err(e) => {
            log(format!("ini の書き方が違う（{e}）。ウィンドウは今のまま"));
            return None;
        }
    };
    match window_size::from_ini(&ini) {
        Ok(None) => {
            log_detail(format!(
                "ini（{origin}）に [window] が無い。ウィンドウは今のまま"
            ));
            None
        }
        Ok(Some(c)) => {
            log_detail(format!("[window] を読んだ（{origin}）: {}", c.describe()));
            Some(c)
        }
        Err(why) => {
            log(format!("{why}。[window] は使わず、ウィンドウは今のまま"));
            None
        }
    }
}

/// 元の `CreateWindowExA`: 呼ぶ時点のゲームの IAT の値（他の道具が IAT を替えていてもそちらを通す）。読めない・どの
/// モジュールにも入っていない値なら、この DLL が import した `CreateWindowExA`。
#[cfg(test)]
fn original() -> CreateWindowFn {
    // 検査のプロセスの `0x48E1FC` は th09.exe の IAT ではない（読めても別の値）ので、読まずに置いた関数か import を使う。
    TEST_ORIG.with(|c| c.get()).unwrap_or(CreateWindowExA)
}

#[cfg(not(test))]
fn original() -> CreateWindowFn {
    let mut b = [0u8; 4];
    if read_own_into(WINDOW_CREATE_IAT, &mut b) {
        let v = u32::from_le_bytes(b);
        if v != 0 && module_of_addr(v).is_some() {
            // SAFETY: IAT の欄には、ローダーが user32 の `CreateWindowExA`（stdcall、12 引数）のアドレスを書いている。値はいずれかのモジュールの中のアドレスと確かめた。ゲーム自身が同じ欄を同じ型で呼んでいる。
            return unsafe { core::mem::transmute::<usize, CreateWindowFn>(v as usize) };
        }
    }
    CreateWindowExA
}

#[derive(Clone, Copy)]
struct Decision {
    cfg: WindowConfig,
    args: CreateArgs,
    orig_style: u32,
}

fn decide(orig_style: u32) -> Option<Decision> {
    let cfg = config()?;
    let args = window_size::plan(&cfg, orig_style, &frame_metrics());
    Some(Decision {
        cfg,
        args,
        orig_style,
    })
}

/// 決めた引数の一文。ウィンドウを作る前には書かず、作った結果（`after_create`）と同じ 1 行にまとめる（行を増やさない）。
fn plan_text(d: &Decision) -> String {
    format!(
        "ゲームの style {:#010X} → {:#010X}、外寸 {}×{}、位置 {}",
        d.orig_style,
        d.args.style,
        d.args.width,
        d.args.height,
        if d.args.x == window_size::CW_USEDEFAULT {
            "今のまま".to_string()
        } else {
            format!("{},{}", d.args.x, d.args.y)
        }
    )
}

/// 差し替え先。ゲームの `0x42D5C2` の `call *0x48e1fc` の代わりに呼ばれる（stdcall、12 引数、`ret 0x30`）。
/// 設定が無ければ引数をそのまま元へ渡す。パニックは受けて、そのときも元の引数で元を呼ぶ。
// 引数の数は Win32 の `CreateWindowExA` が 12 と決めている。
#[allow(clippy::too_many_arguments)]
extern "system" fn create_window_entry(
    ex_style: u32,
    class: *const u8,
    name: *const u8,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: *mut c_void,
    menu: *mut c_void,
    instance: *mut c_void,
    param: *const c_void,
) -> *mut c_void {
    let orig = original();
    let decision = catch_unwind(AssertUnwindSafe(|| decide(style))).unwrap_or(None);
    let (style, x, y, width, height) = match decision {
        Some(d) => (
            d.args.style,
            d.args.x,
            d.args.y,
            d.args.width,
            d.args.height,
        ),
        None => (style, x, y, width, height),
    };
    // SAFETY: `orig` は `CreateWindowExA` と同じ型の関数。引数はゲームが積んだ 12 個のうち、決めた 5 つ（style・位置・大きさ）だけ替える。文字列の 2 つはゲームの `.rdata` のアドレスのまま。
    let hwnd = unsafe {
        orig(
            ex_style, class, name, style, x, y, width, height, parent, menu, instance, param,
        )
    };
    if hwnd.is_null() {
        let plan = decision.map_or(String::new(), |d| format!("。計画: {}", plan_text(&d)));
        log(format!(
            "ウィンドウを作れなかった（元の CreateWindowExA が NULL を返した）{plan}"
        ));
    } else if after_create_enabled() {
        let _ = catch_unwind(AssertUnwindSafe(|| match decision {
            Some(d) => after_create(hwnd, &d),
            None => log_unchanged(hwnd),
        }));
    }
    hwnd
}

/// 作った後の測定と直しをするか。製品では常に true。検査の偽のハンドル（`0x1234` など）は別の本物のウィンドウのハンドルと
/// 偶然同じ値になりうるので、その検査だけ止める。
#[cfg(test)]
fn after_create_enabled() -> bool {
    !TEST_SKIP_AFTER.with(|c| c.get())
}
#[cfg(not(test))]
fn after_create_enabled() -> bool {
    true
}

/// 設定が無いとき（引数をそのまま渡したウィンドウ）の、作った後の style・外寸・client。ウィンドウを変えない実行でも client の画素数を
/// ログに残す（`[window]` の値を決める手がかり）。
fn log_unchanged(hwnd: HWND) {
    // SAFETY: ウィンドウのハンドルと定数の索引だけを渡す。無効なら 0 が返る。
    let style = unsafe { GetWindowLongA(hwnd, GWL_STYLE) } as u32;
    log_detail(format!(
        "ウィンドウを作った（設定なし。引数はそのまま）: style {style:#010X}、外 {:?}、client {:?}",
        window_rect(hwnd).as_ref().map(rect_size),
        client_size(hwnd)
    ));
}

fn rect_size(r: &RECT) -> (i32, i32) {
    (r.right - r.left, r.bottom - r.top)
}

fn client_size(hwnd: HWND) -> Option<(i32, i32)> {
    let mut r = RECT::default();
    // SAFETY: `r` は書ける RECT。`hwnd` は作ったばかりのウィンドウ（無効なら FALSE が返るだけ）。
    (unsafe { GetClientRect(hwnd, &mut r) } != 0).then(|| rect_size(&r))
}

fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    // SAFETY: 同上。
    (unsafe { GetWindowRect(hwnd, &mut r) } != 0).then_some(r)
}

/// ウィンドウを作った後: client を測ってログに残し、指定と違えば外寸を直す。最前面なら最前面にする（前面は奪わない）。
fn after_create(hwnd: HWND, d: &Decision) {
    let want = (d.cfg.client_w, d.cfg.client_h);
    // SAFETY: ウィンドウのハンドルと定数の索引だけを渡す。無効なら 0 が返る。
    let style = unsafe { GetWindowLongA(hwnd, GWL_STYLE) } as u32;
    let client = client_size(hwnd);
    let outer = window_rect(hwnd);
    // 1 回のウィンドウで 1 行にする。直し・最前面が失敗したら簡潔なログにも残す。
    let mut failed = false;
    let mut text = format!(
        "ウィンドウを作った: {}。結果 style {style:#010X}、外 {:?}、client {:?}（指定 {}×{}）",
        plan_text(d),
        outer.as_ref().map(rect_size),
        client,
        want.0,
        want.1
    );
    if let (Some(client), Some(outer)) = (client, outer.as_ref())
        && let Some((w, h)) = window_size::corrected_outer(want, client, rect_size(outer))
    {
        // SAFETY: ウィンドウのハンドルと、大きさだけを変える旗（位置・重なり順・前面は変えない）。
        let ok = unsafe {
            SetWindowPos(
                hwnd,
                core::ptr::null_mut(),
                0,
                0,
                w,
                h,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        } != 0;
        failed |= !ok;
        text.push_str(&format!(
            "。client が指定と違ったので外寸を {w}×{h} に直した（成功: {ok}）。直した後の client {:?}",
            client_size(hwnd)
        ));
    }
    if d.cfg.always_on_top {
        // SAFETY: ウィンドウのハンドルと、位置・大きさ・前面の取り合いのどれも変えない旗（`SWP_NOACTIVATE`）。最前面にするだけ。
        let ok = unsafe {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        } != 0;
        failed |= !ok;
        text.push_str(&format!("。最前面にした（成功: {ok}）"));
    }
    // 直しや最前面が失敗したときだけ簡潔にも残す。
    if failed {
        log(text);
    } else {
        log_detail(text);
    }
}

#[cfg(test)]
mod tests {
    //! 組み上がりの検査（花映塚は使わない）。差し替えの書き込みは検査のプロセスが確保した頁の上のコピーに行い、書き換えた機械語を
    //! 実際に呼んで、stdcall の 12 引数・戻り値・レジスタの保存・後始末が元の `call *mem` と同じに続くかまで見る。
    use super::*;
    use std::sync::Mutex as StdMutex;
    use windows_sys::Win32::Foundation::TRUE;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DestroyWindow, GWL_EXSTYLE, WS_EX_TOPMOST, WS_POPUP,
    };

    struct Page(*mut u8);
    impl Page {
        fn new() -> Page {
            // SAFETY: 新しい 1 頁を確保するだけ（アドレスは OS が選ぶ）。
            let p = unsafe {
                VirtualAlloc(
                    core::ptr::null(),
                    4096,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_EXECUTE_READWRITE,
                )
            };
            assert!(!p.is_null(), "VirtualAlloc");
            Page(p.cast())
        }
        fn addr(&self) -> u32 {
            self.0 as usize as u32
        }
        fn put(&self, at: usize, b: &[u8]) {
            // SAFETY: 確保した 4096 バイトの頁の中（呼び出し元が at + len <= 4096 を守る）。
            unsafe { core::ptr::copy_nonoverlapping(b.as_ptr(), self.0.add(at), b.len()) };
        }
        fn get(&self, at: usize, len: usize) -> Vec<u8> {
            // SAFETY: 同上。
            unsafe { core::slice::from_raw_parts(self.0.add(at), len).to_vec() }
        }
    }
    impl Drop for Page {
        fn drop(&mut self) {
            // SAFETY: `new` で確保した頁を、確保した側が解放する。
            unsafe { VirtualFree(self.0.cast(), 0, MEM_RELEASE) };
        }
    }

    fn site_ctx() -> &'static [u8] {
        WINDOW_CREATE_SITES[0].expected
    }

    fn site_in(page: &Page, at: usize) -> SiteDef {
        SiteDef {
            addr: page.addr() + at as u32,
            ..WINDOW_CREATE_SITES[0]
        }
    }

    #[test]
    fn install_writes_only_when_all_49_bytes_match() {
        let page = Page::new();
        let ctx = site_ctx();
        assert_eq!(ctx.len(), 49);
        for i in 0..ctx.len() {
            let mut bad = ctx.to_vec();
            bad[i] ^= 0xFF;
            page.put(0, &bad);
            let before = page.get(0, 64);
            let site = site_in(&page, 0);
            let r = install_at(
                &mut RealHook::new(),
                &site,
                WINDOW_CREATE_CALL_OFFSET,
                0x1234_5678,
            );
            assert!(r.is_err(), "バイト {i} が違うのに書いた");
            assert_eq!(page.get(0, 64), before, "バイト {i} が違うのに頁が変わった");
        }
        page.put(0, ctx);
        let site = site_in(&page, 0);
        let msg = install_at(
            &mut RealHook::new(),
            &site,
            WINDOW_CREATE_CALL_OFFSET,
            0x1234_5678,
        )
        .expect("全部合えば書く");
        assert!(msg.contains("差し替えた"), "{msg}");
        let now = page.get(0, 49);
        assert_eq!(&now[..43], &ctx[..43]);
        let at = site.addr + 43;
        assert_eq!(&now[43..], &window_size::call_patch(at, 0x1234_5678));
        assert_eq!(now[43], 0xE8);
        assert_eq!(now[48], 0x90);
        let nowhere = SiteDef {
            addr: 0x10,
            ..WINDOW_CREATE_SITES[0]
        };
        let r = install_at(&mut RealHook::new(), &nowhere, WINDOW_CREATE_CALL_OFFSET, 1);
        assert!(r.is_err());
    }

    /// 元の `CreateWindowExA` の代わりに呼ぶ記録係が受けた引数。
    static SEEN: StdMutex<Vec<[usize; 12]>> = StdMutex::new(Vec::new());
    /// SEEN と TEST_* を使う検査を 1 本ずつにする（`SEEN` は全体で 1 つ）。
    static ONE_AT_A_TIME: StdMutex<()> = StdMutex::new(());

    #[allow(clippy::too_many_arguments)] // Win32 の引数の数が 12 と決まっている
    unsafe extern "system" fn recorder(
        a0: u32,
        a1: *const u8,
        a2: *const u8,
        a3: u32,
        a4: i32,
        a5: i32,
        a6: i32,
        a7: i32,
        a8: *mut c_void,
        a9: *mut c_void,
        a10: *mut c_void,
        a11: *const c_void,
    ) -> *mut c_void {
        SEEN.lock().unwrap().push([
            a0 as usize,
            a1 as usize,
            a2 as usize,
            a3 as usize,
            a4 as usize,
            a5 as usize,
            a6 as usize,
            a7 as usize,
            a8 as usize,
            a9 as usize,
            a10 as usize,
            a11 as usize,
        ]);
        0x1234usize as *mut c_void
    }

    /// 書き換えた機械語を呼べる形に組む。ゲームの `0x42D597` 以降と同じ並び（引数の積み込みと call）を、ebx = hInstance・
    /// edi = 幅・eax = 高さ・ecx の元の値を置いてから実行し、戻ったあとで esi / ebx / edi が呼ぶ前と同じかを `flag` に残す。
    fn build_harness(page: &Page, flag_addr: u32) -> usize {
        const EBX: u32 = 0xB1B1_B1B1;
        const ESI: u32 = 0xE5E5_E5E5;
        const EDI: u32 = 646;
        let mut code: Vec<u8> = vec![0x53, 0x56, 0x57]; // push ebx / esi / edi
        code.push(0xBB);
        code.extend_from_slice(&EBX.to_le_bytes()); // mov ebx, hInstance
        code.push(0xBE);
        code.extend_from_slice(&ESI.to_le_bytes()); // mov esi, 呼ぶ前の値
        code.push(0xBF);
        code.extend_from_slice(&EDI.to_le_bytes()); // mov edi, 幅
        code.push(0xB8);
        code.extend_from_slice(&23u32.to_le_bytes()); // mov eax, SM_CYCAPTION の答え
        code.push(0xB9);
        code.extend_from_slice(&3u32.to_le_bytes()); // mov ecx, SM_CYFIXEDFRAME の答え
        let ctx_at = code.len();
        code.extend_from_slice(site_ctx());
        code.extend_from_slice(&[0x89, 0xC1]); // mov ecx, eax
        code.extend_from_slice(&[0x81, 0xF6]);
        code.extend_from_slice(&ESI.to_le_bytes()); // xor esi, ESI
        code.extend_from_slice(&[0x81, 0xF3]);
        code.extend_from_slice(&EBX.to_le_bytes()); // xor ebx, EBX
        code.extend_from_slice(&[0x09, 0xF3]); // or ebx, esi
        code.extend_from_slice(&[0x81, 0xF7]);
        code.extend_from_slice(&EDI.to_le_bytes()); // xor edi, EDI
        code.extend_from_slice(&[0x09, 0xFB]); // or ebx, edi
        code.extend_from_slice(&[0x89, 0x1D]);
        code.extend_from_slice(&flag_addr.to_le_bytes()); // mov [flag], ebx
        code.extend_from_slice(&[0x89, 0xC8]); // mov eax, ecx
        code.extend_from_slice(&[0x5F, 0x5E, 0x5B, 0xC3]); // pop edi / esi / ebx; ret
        page.put(0, &code);
        ctx_at
    }

    fn run_harness(page: &Page) -> usize {
        // SAFETY: `build_harness` が置いた機械語（cdecl の引数なし関数と同じ形で、戻り値は eax）を、確保した実行できる頁の
        // 先頭から呼ぶ。ebx / esi / edi は入口で保存して出口で戻す。
        let f: extern "C" fn() -> usize = unsafe { core::mem::transmute(page.0) };
        f()
    }

    #[test]
    fn patched_call_passes_through_and_applies_the_config() {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let page = Page::new();
        let flag_addr = page.addr() + 1024;
        page.put(1024, &[0xFF; 4]);
        let ctx_at = build_harness(&page, flag_addr);
        let site = site_in(&page, ctx_at);
        install_at(
            &mut RealHook::new(),
            &site,
            WINDOW_CREATE_CALL_OFFSET,
            entry_address(),
        )
        .expect("書ける");
        TEST_ORIG.with(|c| c.set(Some(recorder)));
        TEST_SKIP_AFTER.with(|c| c.set(true));

        TEST_CONFIG.with(|c| c.set(Some(None)));
        SEEN.lock().unwrap().clear();
        let hwnd = run_harness(&page);
        assert_eq!(hwnd, 0x1234, "戻り値");
        assert_eq!(page.get(1024, 4), vec![0; 4], "ebx / esi / edi が壊れた");
        let seen = SEEN.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0],
            [
                0,                  // dwExStyle
                0x0049_021C,        // lpClassName
                0x0049_01E4,        // lpWindowName
                0x100A_0000,        // dwStyle
                0x8000_0000,        // X
                0x8000_0000,        // Y
                646,                // nWidth（edi）
                23 + 3 * 2 + 0x1E0, // nHeight（eax + ecx × 2 + 0x1E0）
                0,                  // hWndParent
                0,                  // hMenu
                0xB1B1_B1B1,        // hInstance（ebx）
                0,                  // lpParam
            ]
        );

        let cfg = WindowConfig {
            client_w: 1280,
            client_h: 960,
            pos: Some((100, 50)),
            title_bar: true,
            always_on_top: false,
        };
        TEST_CONFIG.with(|c| c.set(Some(Some(cfg))));
        SEEN.lock().unwrap().clear();
        page.put(1024, &[0xFF; 4]);
        let hwnd = run_harness(&page);
        assert_eq!(hwnd, 0x1234);
        assert_eq!(
            page.get(1024, 4),
            vec![0; 4],
            "ebx / esi / edi が壊れた（設定あり）"
        );
        let m = frame_metrics();
        let seen = SEEN.lock().unwrap().clone();
        assert_eq!(
            seen[0],
            [
                0,
                0x0049_021C,
                0x0049_01E4,
                0x100A_0000,
                100,
                50,
                (1280 + 2 * m.fixed_frame_x) as usize,
                (960 + m.caption_y + 2 * m.fixed_frame_y) as usize,
                0,
                0,
                0xB1B1_B1B1,
                0,
            ]
        );

        let cfg = WindowConfig {
            title_bar: false,
            pos: None,
            ..cfg
        };
        TEST_CONFIG.with(|c| c.set(Some(Some(cfg))));
        SEEN.lock().unwrap().clear();
        run_harness(&page);
        let seen = SEEN.lock().unwrap().clone();
        assert_eq!(seen[0][3], 0x9000_0000, "style");
        assert_eq!((seen[0][4], seen[0][5]), (0x8000_0000, 0x8000_0000));
        assert_eq!((seen[0][6], seen[0][7]), (1280, 960));

        TEST_ORIG.with(|c| c.set(None));
        TEST_CONFIG.with(|c| c.set(None));
        TEST_SKIP_AFTER.with(|c| c.set(false));
    }

    /// 本物の `CreateWindowExA`（組み込みの `STATIC` ウィンドウ。見えない）で、差し替えの関数が指定どおりの client / 位置 / 最前面のウィンドウを作るか。
    fn create_real(cfg: WindowConfig, game_style: u32) -> (HWND, RECT, (i32, i32), i32) {
        create_real_ex(cfg, game_style, 0)
    }

    fn create_real_ex(
        cfg: WindowConfig,
        game_style: u32,
        ex_style: u32,
    ) -> (HWND, RECT, (i32, i32), i32) {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        TEST_ORIG.with(|c| c.set(None));
        TEST_CONFIG.with(|c| c.set(Some(Some(cfg))));
        let hwnd = create_window_entry(
            ex_style,
            c"STATIC".as_ptr().cast(),
            c"win_test".as_ptr().cast(),
            game_style,
            window_size::CW_USEDEFAULT,
            window_size::CW_USEDEFAULT,
            646,
            509,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null(),
        );
        TEST_CONFIG.with(|c| c.set(None));
        assert!(!hwnd.is_null(), "ウィンドウを作れた");
        let client = client_size(hwnd).expect("client");
        let outer = window_rect(hwnd).expect("rect");
        // SAFETY: 作ったウィンドウのハンドルと定数の索引。
        let ex = unsafe { GetWindowLongA(hwnd, GWL_EXSTYLE) };
        (hwnd, outer, client, ex)
    }

    #[test]
    fn topmost_is_set_on_a_visible_offscreen_window() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW};
        let cfg = WindowConfig {
            client_w: 320,
            client_h: 240,
            pos: Some((-30000, -30000)),
            title_bar: true,
            always_on_top: true,
        };
        let (h, outer, client, ex) =
            create_real_ex(cfg, 0x100A_0000, WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW);
        assert_eq!(client, (320, 240));
        assert_eq!((outer.left, outer.top), (-30000, -30000));
        assert_ne!(ex as u32 & WS_EX_TOPMOST, 0, "最前面");
        destroy(h);
        let (h, _, _, ex) = create_real_ex(
            WindowConfig {
                always_on_top: false,
                ..cfg
            },
            0x100A_0000,
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        );
        assert_eq!(ex as u32 & WS_EX_TOPMOST, 0);
        destroy(h);
    }

    fn destroy(hwnd: HWND) {
        // SAFETY: この検査が作って、このスレッドで持っているウィンドウ。
        assert_eq!(unsafe { DestroyWindow(hwnd) }, TRUE);
    }

    #[test]
    fn real_windows_get_the_requested_client_size() {
        // ゲームの style `0x100A0000` と同じ形（見えないウィンドウにするため WS_VISIBLE だけ外す）。Windows は WS_POPUP / WS_CHILD の
        let game_like = 0x000A_0000;
        let want = WindowConfig {
            client_w: 1001,
            client_h: 563,
            pos: Some((30, 40)),
            title_bar: true,
            always_on_top: true,
        };
        TEST_LINES.with(|l| l.borrow_mut().clear());
        let (h, outer, client, _) = create_real(want, game_like);
        assert_eq!(client, (1001, 563));
        assert_eq!((outer.left, outer.top), (30, 40));
        // 最前面の要求を出して OS が成功を返したことはログに残る。旗は見えないウィンドウでは当てにならない（全件の回で立たない回があった）
        assert!(
            logged()
                .iter()
                .any(|l| l.contains("最前面にした（成功: true）")),
            "最前面の要求を出していない"
        );
        assert_eq!(
            logged().len(),
            1,
            "ウィンドウ 1 枚につき 1 行: {:?}",
            logged()
        );
        assert!(
            rect_size(&outer).0 > 1001 && rect_size(&outer).1 > 563,
            "枠ありは外寸が client より大きい: {:?}",
            rect_size(&outer)
        );
        destroy(h);
        let (h, _, client, ex) = create_real(
            WindowConfig {
                always_on_top: false,
                ..want
            },
            game_like,
        );
        assert_eq!(client, (1001, 563));
        assert_eq!(ex as u32 & WS_EX_TOPMOST, 0, "最前面にしない");
        destroy(h);
        let (h, outer, client, _) = create_real(
            WindowConfig {
                title_bar: false,
                ..want
            },
            game_like,
        );
        assert_eq!(client, (1001, 563));
        assert_eq!(rect_size(&outer), (1001, 563), "枠なしは外寸 = client");
        destroy(h);
        let (h, _, client, _) = create_real(WindowConfig::DEFAULT, game_like);
        assert_eq!(client, (640, 480));
        destroy(h);
        // 外寸の式（枠の分を足す）が必ず外れる style: WS_POPUP は枠が無いので、足した分だけ client が大きく出る → 直しが効く。
        TEST_LINES.with(|l| l.borrow_mut().clear());
        let (h, outer, client, _) = create_real(
            WindowConfig {
                always_on_top: false,
                ..want
            },
            WS_POPUP,
        );
        assert_eq!(client, (1001, 563), "式が外れても指定どおり");
        assert_eq!(rect_size(&outer), (1001, 563));
        assert!(
            logged()
                .iter()
                .any(|l| l.contains("外寸を 1001×563 に直した")),
            "直したことがログに残る: {:?}",
            logged()
        );
        assert_eq!(logged().len(), 1, "直した回も 1 行: {:?}", logged());
        destroy(h);
    }

    fn logged() -> Vec<String> {
        TEST_LINES.with(|l| l.borrow().clone())
    }

    #[test]
    fn the_created_window_is_one_line_with_the_plan_and_the_measured_client() {
        TEST_LINES.with(|l| l.borrow_mut().clear());
        let cfg = WindowConfig {
            client_w: 320,
            client_h: 240,
            pos: Some((-30000, -30000)),
            title_bar: true,
            always_on_top: false,
        };
        let (h, _, client, _) = create_real(cfg, 0x000A_0000);
        assert_eq!(client, (320, 240));
        let lines = logged();
        assert_eq!(lines.len(), 1, "{lines:?}");
        let l = &lines[0];
        for want in [
            "ウィンドウ: ウィンドウを作った: ",
            "ゲームの style 0x000A0000 → ",
            "外寸 ",
            "位置 -30000,-30000",
            "結果 style 0x",
            "client Some((320, 240))",
            "指定 320×240",
        ] {
            assert!(l.contains(want), "{want} が {l} に無い");
        }
        destroy(h);
        TEST_LINES.with(|l| l.borrow_mut().clear());
        TEST_SKIP_AFTER.with(|c| c.set(true));
        TEST_CONFIG.with(|c| c.set(Some(Some(cfg))));
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        unsafe extern "system" fn null_creator(
            _: u32,
            _: *const u8,
            _: *const u8,
            _: u32,
            _: i32,
            _: i32,
            _: i32,
            _: i32,
            _: *mut c_void,
            _: *mut c_void,
            _: *mut c_void,
            _: *const c_void,
        ) -> *mut c_void {
            core::ptr::null_mut()
        }
        TEST_ORIG.with(|c| c.set(Some(null_creator)));
        let hwnd = create_window_entry(
            0,
            c"STATIC".as_ptr().cast(),
            c"w".as_ptr().cast(),
            0x000A_0000,
            window_size::CW_USEDEFAULT,
            window_size::CW_USEDEFAULT,
            646,
            509,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null(),
        );
        TEST_ORIG.with(|c| c.set(None));
        TEST_CONFIG.with(|c| c.set(None));
        TEST_SKIP_AFTER.with(|c| c.set(false));
        assert!(hwnd.is_null());
        let lines = logged();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("ウィンドウを作れなかった")
                && lines[0].contains("計画: ゲームの style"),
            "{lines:?}"
        );
    }

    #[test]
    fn the_refusal_reason_survives_in_the_install_line_and_reaches_the_main_log_queue() {
        let nowhere = SiteDef {
            addr: 0x10,
            ..WINDOW_CREATE_SITES[0]
        };
        let why = install_at(&mut RealHook::new(), &nowhere, WINDOW_CREATE_CALL_OFFSET, 1)
            .expect_err("読めないアドレスは断る");
        let line = install_line(Ok(Err(why.clone())));
        assert!(
            line.starts_with("設置: 差し替えない（ウィンドウは今のまま）: ") && line.contains(&why),
            "{line}"
        );
        assert!(install_line(Ok(Ok("書いた".into()))).contains("書いた"));
        assert!(install_line(Err(Box::new("x"))).contains("パニック"));
        TEST_TO_QUEUE.with(|c| c.set(true));
        log("sidelog_marker_window_queue");
        TEST_TO_QUEUE.with(|c| c.set(false));
        assert!(
            crate::main_log::peek()
                .iter()
                .any(|l| l == "ウィンドウ: sidelog_marker_window_queue"),
            "{:?}",
            crate::main_log::peek()
        );
    }
}
