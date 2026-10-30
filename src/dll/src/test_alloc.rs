//! 検査時だけのアロケータ。スレッドごとに割り当てと解放の回数を数える（`cargo test`の中だけ。成果物のDLLには入らない）。
//! 偽物の`HookEnv`が「止めている間はヒープを使わない」約束を確認するために使う。止めたスレッドがヒープのロックを持っていると自分が止まるため、この約束がある。
//! 確認は「1本目を止めた時と全部を再開した時の回数が同じか」で行う。
//! 検査はスレッドを並べて走るので、数はスレッドローカル変数に持つ。
//! unsafeなのは`GlobalAlloc`がunsafeなトレイトだから。中身は`System`へそのまま渡す。

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    // `const`で初期化され解放の手順も持たない型なので、初めて触るときにも割り当てが起きない（アロケータの中から触ってよい）。
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn bump() {
    // スレッドの終了間際（スレッドローカルストレージの後始末が済んだ後）には数えない。
    let _ = COUNT.try_with(|c| c.set(c.get() + 1));
}

pub fn count() -> u64 {
    COUNT.try_with(Cell::get).unwrap_or(0)
}

struct Counting;

// SAFETY: どの関数も引数をそのまま`System`の同じ関数へ渡して結果を返すだけ。触るのはスレッドローカル変数の数だけで、割り当ても解放もしない。
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: 呼び出し元が`GlobalAlloc::alloc`の約束（大きさ0でない`layout`）を守っている。
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: 同上。
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump();
        // SAFETY: `ptr`はこのアロケータ（＝ `System`）が`layout`で割り当てたもの（呼び出し元の約束）。
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        // SAFETY: 同上。`new_size`の約束も呼び出し元が守っている。
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_allocations_on_this_thread() {
        let before = count();
        let v: Vec<u8> = Vec::with_capacity(64);
        assert!(count() > before);
        let mid = count();
        drop(v);
        assert!(count() > mid);
        let quiet = count();
        let a = [0u8; 32];
        assert_eq!(a.len() + (count() - quiet) as usize, 32);
    }
}
