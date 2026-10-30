//! マッチ終了時のメニューの項目の縁取り（Win32の層。unsafeはここに閉じ込める。1つずつ`// SAFETY:`）。
//!
//! 終わりのメニューの描画のループにある1 VMの描画の呼び出し`0x00435AD2`のrel32を、`DllMain`の中で自前の入口へ向ける。
//! 入口は印のある項目だけ次の順に描く: VMをスタックにコピーして位置と色を変える → 色の演算を頂点の色だけにしてゲームの描画関数で先に描く → 本物のVMを描く（縁取りが文字の下に入る）。
//! 描き方は`higanr_core::end_outline`。
//! 項目の文字の色（`higanr_core::end_pick::item_rgb`）を変えるときは、本物のVMの代わりに色を変えたコピーを描く。
//! ゲームのVMには書かない。ゲームのスレッドで走るので、確保・ロック・I/Oをしない。
//! マッチの終わりの選択のイベントを本ログへ渡すバッファ（`note` → 読み取りの処理の`flush_notes`）もここに置く。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "end_outline_win.rs は 32 ビット（i686）専用（ゲームの thiscall と 32 ビットのアドレスを前提にしている）"
);

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use higanr_core::end_outline::{
    FLASH_FRAMES, ITEMS, ItemMarks, MAX_PASSES, MAX_WIDTH, Pass, Pick, passes,
};
use higanr_core::end_pick::{Mark, Note, Paint, Show, item_rgb};

use crate::hook_win::RealHook;
use crate::main_log;
use crate::overlay_win::{D3DRS_ALPHATESTENABLE, D3DTSS_ALPHAOP, D3DTSS_COLOROP, Dev};
use crate::pad_extra::install_as;
use crate::th09_addr::{
    ANM_DRAW_VM_FN, ANM_FLUSH_FN, ANM_VM_COLOR1, ANM_VM_COLOR2, ANM_VM_POS_X, ANM_VM_POS_Y,
    ANM_VM_SIZE, D3D8_DEVICE_PTR, END_MENU_VMS, END_OUTLINE_SITES,
};

const LOG_LABEL: &str = "終わりのメニューの縁取り";
/// `D3DTOP_SELECTARG2`（色は頂点の色だけ。まとめ描きを描く`0x4396A0`がCOLORARG2を頂点の色にしてある）。
const D3DTOP_SELECTARG2: u32 = 3;
/// `D3DRS_ALPHAREF`・`D3DRS_ALPHAFUNC`と`D3DCMP_GREATEREQUAL`（SDKのd3d8types.h）。
const D3DRS_ALPHAREF: u32 = 24;
const D3DRS_ALPHAFUNC: u32 = 25;
const D3DCMP_GREATEREQUAL: u32 = 7;
const VM_WORDS: usize = (ANM_VM_SIZE / 4) as usize;

/// 1 VMの描画（thiscall・ecx = AnmManager・引数vm）と、まとめ描きを描く関数（thiscall・引数なし）。
type DrawFn = unsafe extern "thiscall" fn(u32, u32);
type FlushFn = unsafe extern "thiscall" fn(u32);

/// 項目ごと・サイドごと（0 = 1P、1 = 2P）の印。種類は 0 = なし・1 = 選択中・2 = 確定で、確定したフレームは`AT`。
static KIND: [[AtomicU8; 2]; ITEMS] = [const { [const { AtomicU8::new(0) }; 2] }; ITEMS];
static AT: [[AtomicU32; 2]; ITEMS] = [const { [const { AtomicU32::new(0) }; 2] }; ITEMS];
static WIDTH: AtomicU8 = AtomicU8::new(2);
/// 縁取る画素の不透明さの下限（0〜255。絵の α がこれ以上の画素だけを縁取る）。
static SOLID: AtomicU8 = AtomicU8::new(0xC0);
/// 見本の表示（`[debug] end_outline_demo`。0 = なし、1・2 = その太さで4つの場面を2秒ずつ回す）。
static DEMO: AtomicU8 = AtomicU8::new(0);
/// 縁取りのカウンタ（項目0を描くたびに1進む）。明滅と確定の光り方に使う。
static FRAME: AtomicU32 = AtomicU32::new(0);
/// 本ログ用: 縁取りを描いた項目の数・最初に読んだ色と α の演算（`COLOROP | ALPHAOP << 8`。未読なら`u32::MAX`）。
static DRAWN: AtomicU32 = AtomicU32::new(0);
static OPS: AtomicU32 = AtomicU32::new(u32::MAX);
/// 項目の文字の色の指示（0 = ゲームの色・1 = 印のある項目だけ白・2 = 決定済みの項目を点滅）と、点滅させる項目・点滅を始めたときのカウンタの値。
static PAINT: AtomicU8 = AtomicU8::new(0);
static PAINT_ITEM: AtomicU8 = AtomicU8::new(0);
static PAINT_AT: AtomicU32 = AtomicU32::new(0);
/// `[debug] end_pick = shadow`: マッチの終わりの選択はステートマシンを動かして本ログに出すだけ（feedも縁取りも変えない）。
static SHADOW: AtomicBool = AtomicBool::new(false);

/// 今のフレームのカウンタ（印を置く側が確定のフレームに使う）。
pub fn frame() -> u32 {
    FRAME.load(Ordering::Relaxed)
}

/// マッチの終わりの選択をシャドウ動作にするかどうか（`[debug] end_pick = shadow`）。
pub fn shadow() -> bool {
    SHADOW.load(Ordering::Relaxed)
}

/// 項目`item`のサイドの印。
/// 確定の点滅は確定したときのカウンタの値から始める。
/// 確定を取り消したら選択中（ゆっくりの明滅）に戻す。
/// 決定済み（`Paint::Decided`）になったら点滅を終えた形にする（後の演出で上書きする）。
fn pick_for(mark: Mark, item: usize, before: Pick, paint: Paint, now: u32) -> Pick {
    match mark {
        Mark::Picking(i) if usize::from(i) == item => Pick::Picking,
        Mark::Confirmed(i) if usize::from(i) == item => match (before, paint) {
            (_, Paint::Decided(_)) => Pick::Confirmed {
                at: now.wrapping_sub(FLASH_FRAMES),
            },
            (keep @ Pick::Confirmed { .. }, _) => keep,
            _ => Pick::Confirmed { at: now },
        },
        _ => Pick::None,
    }
}

/// マッチの終わりの選択の出力を画面へ反映する（ゲームのスレッドから、表示が変わったtickに呼ぶ）。確定の点滅と決定の点滅の開始は、そのtickのカウンタの値にする。
pub fn show(s: &Show) {
    for item in 0..ITEMS {
        for (seat, mark) in s.marks.iter().enumerate() {
            set(
                item,
                seat,
                pick_for(*mark, item, stored(item, seat), s.paint, frame()),
            );
        }
    }
    let (kind, k) = match s.paint {
        Paint::Game => (0, 0),
        Paint::Choosing => (1, 0),
        Paint::Decided(k) => (2, k),
    };
    if kind == 2 && (PAINT.load(Ordering::Relaxed) != 2 || PAINT_ITEM.load(Ordering::Relaxed) != k)
    {
        PAINT_AT.store(frame(), Ordering::Relaxed);
    }
    PAINT_ITEM.store(k, Ordering::Relaxed);
    PAINT.store(kind, Ordering::Relaxed);
}

fn stored_paint() -> Paint {
    match PAINT.load(Ordering::Relaxed) {
        1 => Paint::Choosing,
        2 => Paint::Decided(PAINT_ITEM.load(Ordering::Relaxed)),
        _ => Paint::Game,
    }
}

/// 項目`item`のサイド`seat`（0 = 1P、1 = 2P）の印を置く。ゲームのスレッドから呼ぶ。
pub fn set(item: usize, seat: usize, pick: Pick) {
    let (Some(k), Some(a)) = (
        KIND.get(item).and_then(|r| r.get(seat)),
        AT.get(item).and_then(|r| r.get(seat)),
    ) else {
        return;
    };
    let (kind, at) = match pick {
        Pick::None => (0, 0),
        Pick::Picking => (1, 0),
        Pick::Confirmed { at } => (2, at),
    };
    a.store(at, Ordering::Relaxed);
    k.store(kind, Ordering::Relaxed);
}

fn stored(item: usize, seat: usize) -> Pick {
    match KIND[item][seat].load(Ordering::Relaxed) {
        1 => Pick::Picking,
        2 => Pick::Confirmed {
            at: AT[item][seat].load(Ordering::Relaxed),
        },
        _ => Pick::None,
    }
}

/// 見本の4つの場面（2秒ずつ）: 2人とも選択中 → 1Pが再戦で確定 → 2Pがキャラを変更で確定 → 2人が同じ項目。
fn demo_marks(item: usize, frame: u32) -> ItemMarks {
    const SCENE: u32 = 120;
    let t = frame % (SCENE * 4);
    let start = frame - t;
    let scene = t / SCENE;
    let c = |s: u32| Pick::Confirmed {
        at: start + s * SCENE,
    };
    let (p1, p2) = match (scene, item) {
        (0, 0) => (Pick::Picking, Pick::None),
        (0, 1) => (Pick::None, Pick::Picking),
        (1, 0) => (c(1), Pick::None),
        (1, 2) => (Pick::None, Pick::Picking),
        (2, 0) => (c(1), Pick::None),
        (2, 1) => (Pick::None, c(2)),
        (3, 1) => (c(3), Pick::Picking),
        _ => (Pick::None, Pick::None),
    };
    ItemMarks { p1, p2 }
}

fn marks_for(item: usize, frame: u32) -> ItemMarks {
    if DEMO.load(Ordering::Relaxed) != 0 {
        return demo_marks(item, frame);
    }
    ItemMarks {
        p1: stored(item, 0),
        p2: stored(item, 1),
    }
}

/// ゲームから呼ばれる入口。`0x00435AD2`のrel32がここへ向く。引数は元の描画関数と同じ。
/// 縁取りが描けなくても・パニックしても、項目は必ず描く（色を変えたコピーを描けなかったら本物のVM）。
pub extern "thiscall" fn end_outline_entry(this: u32, vm: u32) {
    let _ = catch_unwind(AssertUnwindSafe(|| outline(this, vm)));
    if catch_unwind(AssertUnwindSafe(|| paint(this, vm))).unwrap_or(false) {
        return;
    }
    // SAFETY: 差し替える前と同じ関数を同じ引数で呼ぶ（`ANM_DRAW_VM_FN`の先頭は設置のときに照合した）。
    unsafe { core::mem::transmute::<usize, DrawFn>(ANM_DRAW_VM_FN as usize)(this, vm) }
}

/// 色の指示があれば色1だけを変えたVMのコピーを本物の代わりに描いてtrueを返す（ゲーム自身も状態1〜3で色1だけを毎フレーム書く。α はVMの値のまま）。
fn paint(this: u32, vm: u32) -> bool {
    let off = vm.wrapping_sub(END_MENU_VMS);
    let item = off / ANM_VM_SIZE;
    if !off.is_multiple_of(ANM_VM_SIZE) || item >= ITEMS as u32 || this == 0 {
        return false;
    }
    let since = frame().wrapping_sub(PAINT_AT.load(Ordering::Relaxed));
    let marked = KIND[item as usize]
        .iter()
        .any(|k| k.load(Ordering::Relaxed) != 0);
    let Some(rgb) = item_rgb(stored_paint(), item as u8, since, marked) else {
        return false;
    };
    let mut copy = [0u32; VM_WORDS];
    // SAFETY: `vm`は終わりのメニューのobjectの中の3つのVMのどれか（上で範囲と並びを見た。ゲームの静的な領域で、0x2A4バイト読み取り可能）。
    unsafe {
        core::ptr::copy_nonoverlapping(vm as usize as *const u32, copy.as_mut_ptr(), VM_WORDS)
    };
    let c = (ANM_VM_COLOR1 / 4) as usize;
    copy[c] = (copy[c] & 0xFF00_0000) | rgb;
    // SAFETY: 本物と同じ描画関数を、スタック上の同じ形のコピーで呼ぶ（縁取りと同じ）。
    // `0x43A950`と呼び先の`0x434370`・`0x43A0F0`はVMを読むだけ。
    // 書くのはAnmManagerのフィールド（`0x0043A449`など）なので、本物のVMを描かなくてもVMの中身は変わらない。
    // ゲームのスレッドで描画の最中に呼ぶ。
    unsafe {
        core::mem::transmute::<usize, DrawFn>(ANM_DRAW_VM_FN as usize)(
            this,
            copy.as_ptr() as usize as u32,
        )
    };
    true
}

fn outline(this: u32, vm: u32) {
    let off = vm.wrapping_sub(END_MENU_VMS);
    let item = (off / ANM_VM_SIZE) as usize;
    if !off.is_multiple_of(ANM_VM_SIZE) || item >= ITEMS || this == 0 {
        return;
    }
    if item == 0 {
        FRAME.fetch_add(1, Ordering::Relaxed);
    }
    let frame = FRAME.load(Ordering::Relaxed);
    let marks = marks_for(item, frame);
    if marks.is_empty() {
        return;
    }
    let mut list = [Pass::default(); MAX_PASSES];
    let n = passes(marks, frame, WIDTH.load(Ordering::Relaxed), &mut list);
    // SAFETY: `D3D8_DEVICE_PTR`はゲームの静的なフィールド（4バイト。描画の最中なのでデバイスは作られている）。
    let dev = unsafe { (D3D8_DEVICE_PTR as usize as *const u32).read_unaligned() };
    if n == 0 || dev == 0 {
        return;
    }
    let dev = Dev(dev);
    let mut copy = [0u32; VM_WORDS];
    // SAFETY: `vm`は終わりのメニューのobjectの中の3つのVMのどれか（上で範囲と並びを見た。ゲームの静的な領域で、0x2A4バイト読み取り可能）。
    unsafe {
        core::ptr::copy_nonoverlapping(vm as usize as *const u32, copy.as_mut_ptr(), VM_WORDS)
    };
    let word = |o: u32| (o / 4) as usize;
    let (x, y) = (
        f32::from_bits(copy[word(ANM_VM_POS_X)]),
        f32::from_bits(copy[word(ANM_VM_POS_Y)]),
    );
    // 文字が消えかけているとき（出てくる途中など）は縁取りも同じだけ薄くする。
    let vm_alpha = copy[word(ANM_VM_COLOR1)] >> 24;
    // SAFETY: どれもゲームのスレッドで描画の最中に呼ぶ。`this`はAnmManager（ゲームが渡した値）。`dev`は生きているデバイス。
    // flushはまとめ描きを今描くだけ（ゲーム自身もテクスチャや合成の切り替えの前に呼ぶ）。色の演算は描き終えたら元の値へ戻す。
    unsafe {
        let flush = core::mem::transmute::<usize, FlushFn>(ANM_FLUSH_FN as usize);
        let draw = core::mem::transmute::<usize, DrawFn>(ANM_DRAW_VM_FN as usize);
        let (mut color_op, mut alpha_op) = (0u32, 0u32);
        if dev.get_texture_stage_state(0, D3DTSS_COLOROP, &mut color_op) < 0 {
            return;
        }
        let _ = dev.get_texture_stage_state(0, D3DTSS_ALPHAOP, &mut alpha_op);
        let _ = OPS.compare_exchange(
            u32::MAX,
            color_op | alpha_op << 8,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        let (mut test_on, mut test_ref, mut test_func) = (0u32, 0u32, 0u32);
        let _ = dev.get_render_state(D3DRS_ALPHATESTENABLE, &mut test_on);
        let _ = dev.get_render_state(D3DRS_ALPHAREF, &mut test_ref);
        let _ = dev.get_render_state(D3DRS_ALPHAFUNC, &mut test_func);
        flush(this);
        dev.set_texture_stage_state(0, D3DTSS_COLOROP, D3DTOP_SELECTARG2);
        // 項目の絵には英語の小さい文字と薄い影が半透明で入っているので、不透明に近い画素（日本語の文字）だけを縁取る。
        // しきい値は縁取りの α に比例させる（明滅で薄くしても同じ画素が残る）。
        dev.set_render_state(D3DRS_ALPHATESTENABLE, 1);
        dev.set_render_state(D3DRS_ALPHAFUNC, D3DCMP_GREATEREQUAL);
        let solid = u32::from(SOLID.load(Ordering::Relaxed));
        let mut last_a = u32::MAX;
        for p in &list[..n] {
            let a = (p.argb >> 24) * vm_alpha / 255;
            if a != last_a {
                flush(this);
                dev.set_render_state(D3DRS_ALPHAREF, (a * solid / 255).max(1));
                last_a = a;
            }
            copy[word(ANM_VM_POS_X)] = (x + f32::from(p.dx)).to_bits();
            copy[word(ANM_VM_POS_Y)] = (y + f32::from(p.dy)).to_bits();
            let argb = a << 24 | (p.argb & 0x00FF_FFFF);
            copy[word(ANM_VM_COLOR1)] = argb;
            copy[word(ANM_VM_COLOR2)] = argb;
            draw(this, copy.as_ptr() as usize as u32);
        }
        flush(this);
        dev.set_texture_stage_state(0, D3DTSS_COLOROP, color_op);
        dev.set_render_state(D3DRS_ALPHAREF, test_ref);
        dev.set_render_state(D3DRS_ALPHAFUNC, test_func);
        dev.set_render_state(D3DRS_ALPHATESTENABLE, test_on);
    }
    DRAWN.fetch_add(1, Ordering::Relaxed);
}

/// 縁取りの太さと見本の表示をiniから設定する（`[debug] end_outline_demo = 0 / 1 / 2`）。起動後にゲームのスレッドの外で1回呼ぶ。
pub fn configure(ini_text: Option<&str>) {
    let ini = ini_text.and_then(|t| higanr_core::ini::parse(t).ok());
    let num = |key: &str| {
        ini.as_ref()
            .and_then(|i| i.get("debug", key))
            .and_then(|v| v.parse::<u8>().ok())
    };
    let demo = num("end_outline_demo").unwrap_or(0).min(MAX_WIDTH);
    if let Some(s) = num("end_outline_solid") {
        SOLID.store(s, Ordering::Relaxed);
    }
    if demo != 0 {
        WIDTH.store(demo, Ordering::Relaxed);
        main_log::push(format!(
            "{LOG_LABEL}: 見本の表示（太さ {demo} 画素・不透明さの下限 {}。4 つの場面を 2 秒ずつ）。",
            SOLID.load(Ordering::Relaxed)
        ));
    }
    DEMO.store(demo, Ordering::Relaxed);
    let shadow = ini
        .as_ref()
        .and_then(|i| i.get("debug", "end_pick"))
        .is_some_and(|v| v.trim() == "shadow");
    if shadow {
        main_log::push(format!(
            "{PICK_LABEL}: 記録のみ（入力・縁取り・音は変えません）。"
        ));
    }
    SHADOW.store(shadow, Ordering::Relaxed);
}

// ---- マッチの終わりの選択のイベントの本ログ（ゲームのスレッドが書き、読み取りの処理が1秒ごとに本ログへ出す）----

const PICK_LABEL: &str = "マッチの終わりの選択";
/// イベント (seg, 誰, 項目, frame)のバッファ。
/// 決定は1マッチに1つ。サイドごとの確定（シャドウ動作だけ）も数回。
/// そのため読み取りの処理の1秒の間に溢れることはまず無い（溢れたら古い方を捨てて数える）。
const NOTE_SLOTS: usize = 32;
static NOTES: [AtomicU64; NOTE_SLOTS] = [const { AtomicU64::new(0) }; NOTE_SLOTS];
/// 書いた数（ゲームのスレッドだけがReleaseで進める）と、読んだ数（読み取りの処理だけが進める）・溢れた数・決まった数。
static NOTES_WRITTEN: AtomicU32 = AtomicU32::new(0);
static NOTES_READ: AtomicU32 = AtomicU32::new(0);
static NOTES_LOST: AtomicU32 = AtomicU32::new(0);
static DECIDED_COUNT: AtomicU32 = AtomicU32::new(0);

/// バッファの1枠の形: seg（16 bit）・誰（0 = 1Pの確定、1 = 2Pの確定、2 = 決定）・項目・frame。
fn pack_note(seg: u16, note: Note, frame: u32) -> u64 {
    let (who, item) = match note {
        Note::Confirmed { seat, item } => (seat.min(1), item),
        Note::Decided(item) => (2, item),
    };
    u64::from(seg) << 48 | u64::from(who) << 40 | u64::from(item) << 32 | u64::from(frame)
}

/// イベントをバッファへ書き込む（ゲームのスレッド。確保しない）。
pub fn note(seg: u16, n: Note, frame: u32) {
    if matches!(n, Note::Decided(_)) {
        DECIDED_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    let w = NOTES_WRITTEN.load(Ordering::Relaxed);
    NOTES[w as usize % NOTE_SLOTS].store(pack_note(seg, n, frame), Ordering::Relaxed);
    NOTES_WRITTEN.store(w.wrapping_add(1), Ordering::Release);
}

fn item_name(item: u8) -> &'static str {
    match item {
        0 => "再戦",
        1 => "キャラを変更",
        _ => "リプレイを保存して終了",
    }
}

/// バッファの1枠の本ログの行。
fn note_line(v: u64, shadow: bool) -> String {
    let (seg, who, item, frame) = (v >> 48, (v >> 40) & 0xFF, (v >> 32) as u8, v as u32);
    let only = if shadow { "（記録のみ）" } else { "" };
    match who {
        2 => format!(
            "{PICK_LABEL}: seg {seg}・frame {frame} で「{}」に決まりました{only}。",
            item_name(item)
        ),
        s => format!(
            "{PICK_LABEL}: seg {seg}・frame {frame} で {}P が「{}」を確定しました{only}。",
            s + 1,
            item_name(item)
        ),
    }
}

/// バッファの中身を本ログへ出す（読み取りの処理から1秒ごと）。
pub fn flush_notes() {
    let w = NOTES_WRITTEN.load(Ordering::Acquire);
    let mut r = NOTES_READ.load(Ordering::Relaxed);
    let behind = w.wrapping_sub(r);
    if behind as usize > NOTE_SLOTS {
        NOTES_LOST.fetch_add(behind - NOTE_SLOTS as u32, Ordering::Relaxed);
        r = w.wrapping_sub(NOTE_SLOTS as u32);
    }
    let sh = shadow();
    while r != w {
        let v = NOTES[r as usize % NOTE_SLOTS].load(Ordering::Relaxed);
        main_log::push(note_line(v, sh));
        r = r.wrapping_add(1);
    }
    NOTES_READ.store(r, Ordering::Relaxed);
}

/// 本ログの行（縁取りを描いた数・最初に読んだ色と α の演算・選択の決定数）。
pub fn stats_text() -> String {
    let ops = OPS.load(Ordering::Relaxed);
    let ops = if ops == u32::MAX {
        "未読".to_string()
    } else {
        format!("COLOROP {} / ALPHAOP {}", ops & 0xFF, ops >> 8 & 0xFF)
    };
    let lost = match NOTES_LOST.load(Ordering::Relaxed) {
        0 => String::new(),
        n => format!("・本ログへ渡せなかった {n} 回"),
    };
    format!(
        "\n{LOG_LABEL}: 描いた項目 {} 回（{ops}）\n{PICK_LABEL}: 決まった {} 回{lost}",
        DRAWN.load(Ordering::Relaxed),
        DECIDED_COUNT.load(Ordering::Relaxed)
    )
}

/// `DllMain`（`DLL_PROCESS_ATTACH`）から呼ぶ。4行を照合して呼び出し1つを入口へ向ける。1行でも合わなければ1バイトも書かない。
pub fn on_attach() {
    let r = catch_unwind(AssertUnwindSafe(|| {
        let mut env = RealHook::new();
        let entry: extern "thiscall" fn(u32, u32) = end_outline_entry;
        let report = install_as(
            &mut env,
            END_OUTLINE_SITES,
            entry as usize as u32,
            LOG_LABEL,
        );
        for l in &report.lines {
            main_log::push_when(!report.complete(), l.clone());
        }
    }));
    if r.is_err() {
        main_log::push(format!("{LOG_LABEL}: 設置の途中でパニックを受けました。"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_confirm_blinks_from_its_frame_and_a_cancel_or_the_decision_overrides_it() {
        let lit = Paint::Choosing;
        let now = 1_000;
        // 確定したtickから点滅を始め、続くtickは同じ始まりを保つ。
        let c = pick_for(Mark::Confirmed(1), 1, Pick::Picking, lit, now);
        assert_eq!(c, Pick::Confirmed { at: now });
        assert_eq!(pick_for(Mark::Confirmed(1), 1, c, lit, now + 7), c);
        // 点滅の途中で取り消したらすぐ選択中（ゆっくりの明滅）になる。
        assert_eq!(
            pick_for(Mark::Picking(1), 1, c, lit, now + 7),
            Pick::Picking
        );
        // 決定済みになったら点滅を終えた形（サイドの色のまま）になる。
        let done = pick_for(Mark::Confirmed(1), 1, c, Paint::Decided(1), now + 7);
        assert_eq!(
            done,
            Pick::Confirmed {
                at: now + 7 - FLASH_FRAMES
            }
        );
        let rgb = higanr_core::end_outline::RGB_2P;
        assert_eq!(
            higanr_core::end_outline::color(done, rgb, now + 7),
            Some(0xFF00_0000 | rgb)
        );
        // ほかの項目の印は無し。
        assert_eq!(pick_for(Mark::Confirmed(1), 0, c, lit, now), Pick::None);
    }

    #[test]
    fn the_demo_walks_four_scenes_and_confirms_at_the_scene_start() {
        let at = |m: ItemMarks| match (m.p1, m.p2) {
            (Pick::Confirmed { at }, _) | (_, Pick::Confirmed { at }) => Some(at),
            _ => None,
        };
        let f0 = 480 * 7;
        assert_eq!(demo_marks(0, f0 + 10).p1, Pick::Picking);
        assert_eq!(demo_marks(1, f0 + 10).p2, Pick::Picking);
        assert!(demo_marks(2, f0 + 10).is_empty());
        assert_eq!(at(demo_marks(0, f0 + 130)), Some(f0 + 120));
        assert_eq!(demo_marks(2, f0 + 130).p2, Pick::Picking);
        assert_eq!(at(demo_marks(1, f0 + 250)), Some(f0 + 240));
        let shared = demo_marks(1, f0 + 370);
        assert_eq!(
            (shared.p1, shared.p2),
            (Pick::Confirmed { at: f0 + 360 }, Pick::Picking)
        );
    }

    /// 印と色の指示は静的変数なので、置く・消す検査はこの1本にまとめる（並べて走らせると取り合う）。
    #[test]
    fn set_and_show_store_each_seat_keep_the_confirm_frame_and_start_the_blink_at_the_decide() {
        set(2, 1, Pick::Confirmed { at: 77 });
        assert_eq!(stored(2, 1), Pick::Confirmed { at: 77 });
        assert_eq!(stored(2, 0), Pick::None);
        set(2, 1, Pick::None);
        assert_eq!(stored(2, 1), Pick::None);
        // 範囲の外は何もしない。
        set(ITEMS, 0, Pick::Picking);
        set(0, 2, Pick::Picking);

        let all = || -> Vec<Pick> {
            (0..ITEMS)
                .flat_map(|i| [stored(i, 0), stored(i, 1)])
                .collect()
        };
        FRAME.store(100, Ordering::Relaxed);
        let s = |m1, m2, paint| Show {
            marks: [m1, m2],
            paint,
        };
        show(&s(Mark::Picking(0), Mark::Confirmed(2), Paint::Choosing));
        let c100 = Pick::Confirmed { at: 100 };
        assert_eq!(
            all(),
            [
                Pick::Picking,
                Pick::None,
                Pick::None,
                Pick::None,
                Pick::None,
                c100
            ]
        );
        assert_eq!(stored_paint(), Paint::Choosing);
        // 同じ確定のままなら光り直さない。
        FRAME.store(105, Ordering::Relaxed);
        show(&s(Mark::Picking(1), Mark::Confirmed(2), Paint::Choosing));
        assert_eq!(stored(2, 1), c100);
        assert_eq!((stored(0, 0), stored(1, 0)), (Pick::None, Pick::Picking));
        // 選び直して確定し直したら、そのtickのカウンタの値から光る。
        show(&s(Mark::Picking(1), Mark::Picking(2), Paint::Choosing));
        FRAME.store(110, Ordering::Relaxed);
        show(&s(Mark::Picking(1), Mark::Confirmed(2), Paint::Choosing));
        assert_eq!(stored(2, 1), Pick::Confirmed { at: 110 });
        // 点滅は決定に変わったtickから数え、同じ項目の間は数え直さない。
        FRAME.store(120, Ordering::Relaxed);
        show(&s(
            Mark::Confirmed(1),
            Mark::Confirmed(2),
            Paint::Decided(2),
        ));
        FRAME.store(124, Ordering::Relaxed);
        show(&s(
            Mark::Confirmed(1),
            Mark::Confirmed(2),
            Paint::Decided(2),
        ));
        assert_eq!(
            (stored_paint(), PAINT_AT.load(Ordering::Relaxed)),
            (Paint::Decided(2), 120)
        );
        show(&s(
            Mark::Confirmed(1),
            Mark::Confirmed(2),
            Paint::Decided(0),
        ));
        assert_eq!(PAINT_AT.load(Ordering::Relaxed), 124);
        show(&Show::NONE);
        assert!(all().iter().all(|p| *p == Pick::None));
        assert_eq!(stored_paint(), Paint::Game);
    }

    #[test]
    fn a_packed_note_reads_back_into_its_log_line() {
        let d = note_line(pack_note(3, Note::Decided(1), 1234), false);
        assert_eq!(
            d,
            "マッチの終わりの選択: seg 3・frame 1234 で「キャラを変更」に決まりました。"
        );
        let c = note_line(
            pack_note(65_535, Note::Confirmed { seat: 1, item: 2 }, u32::MAX),
            true,
        );
        assert_eq!(
            c,
            "マッチの終わりの選択: seg 65535・frame 4294967295 で 2P が「リプレイを保存して終了」を確定しました（記録のみ）。"
        );
        assert!(
            note_line(pack_note(0, Note::Confirmed { seat: 0, item: 0 }, 0), true)
                .contains("1P が「再戦」")
        );
    }
}
