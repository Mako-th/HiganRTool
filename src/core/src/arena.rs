//! 割り当て器（pure）。マッチの中のゲームのスレッドの確保を、DLL が予約した 1 つの領域から配る。
//!
//! 自前にする理由: CRT のヒープのままだと、コピーを書き戻してもヒープの「どこが空いているか」は戻らず、書き戻した本体が
//! 解放済みの塊を指す。確保をこの領域に集め、割り当て器の状態（`ArenaState`）と領域の中身をいっしょにコピーして書き戻せば、
//! 確保と解放も巻き戻る（giuroll の「確保を追跡して巻き戻す」考えを参考にした。コードはコピーしていない）。
//!
//! - 大きさの級（16 << c バイト。c = 0..CLASSES）ごとの空きの列と、上へ伸ばすだけの `top`。級の中の塊は使い回すだけで、
//!   つなげも分けもしない。同じ順の確保・解放には必ず同じ位置を返す（巻き戻して進め直してもポインタの値まで同じ）。
//! - 塊のヘッダ（16 バイト）は領域の中に置く: 印・級・求められた大きさ・4 語目。4 語目は、使っている間は確保した者の札
//!   （DLL は呼び出し元のアドレス）、空きの列にある間は列の次。中身は 16 バイト境界。札をヘッダに置くのは、比べから外す所を持つ塊を
//!   見分けるため。ヘッダは領域の中なので、コピーと書き戻しで札もいっしょに戻る。
//! - 解放した塊は、ヘッダの後ろの中身を級の大きさ全部 0 で埋める。ゲームは解放した塊を読まない（推定）ので、前の持ち主の中身が残って
//!   「同じ状態なのに中身が違う」と見えるのを消し、期間の途中で解放された塊を比べから外さずに済ませる（相手どうしの間でも同じになる）。
//! - 領域の中身には `ArenaMem` を通してだけ触る。失敗は `None` / `Err` で返しパニックしない（ゲームのスレッドの確保の
//!   中で呼ばれる）。確保できなければ呼び出し側は CRT へ素通しする。

pub const ALIGN: u32 = 16;
pub const HEADER: u32 = 16;
/// 級の数。級 c の中身は `16 << c` バイト（c = 27 で 2 GiB。領域がそれより小さければ `top` の検査で断る）。
pub const CLASSES: usize = 28;
pub const NONE: u32 = u32::MAX;

/// 使っている塊の印（ヘッダの 1 語目）。解放済み・壊れたヘッダと見分けるためだけの値。
const MAGIC_USED: u32 = 0x5242_5553;
const MAGIC_FREE: u32 = 0x5242_4546;

// ヘッダの語の位置（塊の先頭からのバイト）。
const H_MAGIC: u32 = 0;
const H_CLASS: u32 = 4;
const H_SIZE: u32 = 8;
/// 空きの列にある間は列の次、使っている間は札（`alloc_tagged` の `tag`）。同じ語を使うのは、ヘッダの大きさと中身の境界を変えないため。
const H_NEXT: u32 = 12;
const H_TAG: u32 = H_NEXT;

/// 領域の中身（4 バイトの語で読み書き）。`off` は領域の先頭からのバイトで、4 の倍数・`len` 未満だけを渡す。
// 領域は空にならない。
#[allow(clippy::len_without_is_empty)]
pub trait ArenaMem {
    fn len(&self) -> u32;
    fn read(&self, off: u32) -> u32;
    fn write(&mut self, off: u32, v: u32);
    /// `[off, off + len)` を 0 で埋める（4 の倍数で領域に収まる所だけ）。既定は 1 語ずつの `write`。
    /// 本物は塊で埋める（ゲームのスレッドで走るので、語ごとの呼び出しを重ねない）。
    fn zero(&mut self, off: u32, len: u32) {
        for o in (off..off + len).step_by(4) {
            self.write(o, 0);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockInfo {
    /// ヘッダの位置。中身の位置は `at + HEADER`。
    pub at: u32,
    /// 中身の級のバイト数（`16 << c`）。
    pub payload_bytes: u32,
    pub used: bool,
    /// 確保のときに求められた大きさ（空きの塊でも残る）。
    pub size: u32,
    /// 使っている塊の札（空きの塊では `None`）。
    pub tag: Option<u32>,
}

/// 割り当て器の状態（これと領域の `[0, top)` をコピーすれば、巻き戻せる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaState {
    /// 次に上へ伸ばす位置。`[top, len)` は一度も配っていない（DLL は 0 のまま保つ）。
    pub top: u32,
    pub heads: [u32; CLASSES],
    /// 使っている塊の数と、その求められた大きさの和。
    pub live_blocks: u32,
    pub live_bytes: u32,
    /// これまでの確保・解放の回数（巻き戻すと戻る。期間の中の回数を差で取る）。
    pub allocs: u32,
    pub frees: u32,
    /// 空きの列の先頭が壊れていた回数。壊れた列は捨てて上へ伸ばす。
    pub broken_lists: u32,
}

impl Default for ArenaState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeError {
    Misaligned,
    OutOfRange,
    /// 二重の解放か、塊でない所。
    NotInUse,
    BadHeader,
}

pub fn class_of(size: u32) -> Option<usize> {
    let need = size.max(1);
    (0..CLASSES).find(|&c| class_bytes(c) >= need)
}

pub fn class_bytes(c: usize) -> u32 {
    ALIGN << c
}

impl ArenaState {
    pub const fn new() -> Self {
        ArenaState {
            top: 0,
            heads: [NONE; CLASSES],
            live_blocks: 0,
            live_bytes: 0,
            allocs: 0,
            frees: 0,
            broken_lists: 0,
        }
    }

    /// `size` バイトを配り、中身の位置を返す。領域が足りなければ何も書かず `None`。札は `NONE`。
    pub fn alloc<M: ArenaMem>(&mut self, mem: &mut M, size: u32) -> Option<u32> {
        self.alloc_tagged(mem, size, NONE)
    }

    /// `alloc` と同じに配り、ヘッダに札 `tag` を置く（`for_each_used` が返す）。
    pub fn alloc_tagged<M: ArenaMem>(&mut self, mem: &mut M, size: u32, tag: u32) -> Option<u32> {
        let c = class_of(size)?;
        let block = HEADER.checked_add(class_bytes(c))?;
        let head = self.heads[c];
        let at = if head != NONE && self.is_block(mem, head, c, MAGIC_FREE) {
            self.heads[c] = mem.read(head + H_NEXT);
            head
        } else {
            if head != NONE {
                // 列の先頭のヘッダが壊れている（ゲームが塊の外へ書いた）。この列は捨て、上へ伸ばす。
                self.heads[c] = NONE;
                self.broken_lists = self.broken_lists.wrapping_add(1);
            }
            let end = self.top.checked_add(block)?;
            if end > mem.len() {
                return None;
            }
            let at = self.top;
            self.top = end;
            at
        };
        mem.write(at + H_MAGIC, MAGIC_USED);
        mem.write(at + H_CLASS, c as u32);
        mem.write(at + H_SIZE, size);
        mem.write(at + H_TAG, tag);
        self.live_blocks = self.live_blocks.wrapping_add(1);
        self.live_bytes = self.live_bytes.wrapping_add(size);
        self.allocs = self.allocs.wrapping_add(1);
        Some(at + HEADER)
    }

    /// 塊を空きの列へ戻し、中身を級の大きさ全部 0 で埋める。ヘッダが合わなければ何も書かずに断る。
    pub fn free<M: ArenaMem>(&mut self, mem: &mut M, payload: u32) -> Result<(), FreeError> {
        if !payload.is_multiple_of(ALIGN) || payload < HEADER {
            return Err(FreeError::Misaligned);
        }
        if payload >= self.top {
            return Err(FreeError::OutOfRange);
        }
        let at = payload - HEADER;
        if mem.read(at + H_MAGIC) != MAGIC_USED {
            return Err(FreeError::NotInUse);
        }
        let c = mem.read(at + H_CLASS) as usize;
        if !self.is_block(mem, at, c, MAGIC_USED) {
            return Err(FreeError::BadHeader);
        }
        let size = mem.read(at + H_SIZE);
        // 級の大きさ全部を埋める（求められた大きさの後ろの余りも、使い回されても書かれない）。ヘッダの語は下で書く。
        mem.zero(at + HEADER, class_bytes(c));
        mem.write(at + H_MAGIC, MAGIC_FREE);
        mem.write(at + H_NEXT, self.heads[c]);
        self.heads[c] = at;
        self.live_blocks = self.live_blocks.wrapping_sub(1);
        self.live_bytes = self.live_bytes.wrapping_sub(size);
        self.frees = self.frees.wrapping_add(1);
        Ok(())
    }

    /// `at` に級 `c` の塊のヘッダがあり、印が `magic` で、塊が `top` の中に収まるか。
    fn is_block<M: ArenaMem>(&self, mem: &M, at: u32, c: usize, magic: u32) -> bool {
        if c >= CLASSES || !at.is_multiple_of(ALIGN) {
            return false;
        }
        let Some(end) = at
            .checked_add(HEADER)
            .and_then(|x| x.checked_add(class_bytes(c)))
        else {
            return false;
        };
        end <= self.top && mem.read(at + H_MAGIC) == magic && mem.read(at + H_CLASS) as usize == c
    }

    /// 使っている塊を先頭から順に `f(中身の位置, 求められた大きさ, 札)` で渡す。ヘッダが壊れた塊に当たったらそこで止めて
    /// `false` を返す（その先は渡さない。比べから外す塊が減るだけで、食い違いを隠す向きには働かない）。
    pub fn for_each_used<M: ArenaMem>(&self, mem: &M, mut f: impl FnMut(u32, u32, u32)) -> bool {
        let mut at = 0u32;
        while at < self.top {
            let magic = mem.read(at + H_MAGIC);
            let c = mem.read(at + H_CLASS) as usize;
            if (magic != MAGIC_USED && magic != MAGIC_FREE) || !self.is_block(mem, at, c, magic) {
                return false;
            }
            if magic == MAGIC_USED {
                f(at + HEADER, mem.read(at + H_SIZE), mem.read(at + H_TAG));
            }
            at += HEADER + class_bytes(c);
        }
        true
    }

    /// ヘッダの位置 `at` の塊 1 つの情報。`at` が `top` 以上かヘッダが壊れていれば `None`。次のヘッダは `at + HEADER + payload_bytes`。
    /// 空きの塊では札が列の次と同じ語に置き換わるので `None`。
    pub fn block_at<M: ArenaMem>(&self, mem: &M, at: u32) -> Option<BlockInfo> {
        if at >= self.top {
            return None;
        }
        let magic = mem.read(at + H_MAGIC);
        let c = mem.read(at + H_CLASS) as usize;
        if (magic != MAGIC_USED && magic != MAGIC_FREE) || !self.is_block(mem, at, c, magic) {
            return None;
        }
        let used = magic == MAGIC_USED;
        Some(BlockInfo {
            at,
            payload_bytes: class_bytes(c),
            used,
            size: mem.read(at + H_SIZE),
            tag: used.then(|| mem.read(at + H_TAG)),
        })
    }

    pub fn to_words(&self) -> [u32; STATE_WORDS] {
        let mut w = [0u32; STATE_WORDS];
        w[0] = self.top;
        w[1..1 + CLASSES].copy_from_slice(&self.heads);
        w[1 + CLASSES] = self.live_blocks;
        w[2 + CLASSES] = self.live_bytes;
        w[3 + CLASSES] = self.allocs;
        w[4 + CLASSES] = self.frees;
        w[5 + CLASSES] = self.broken_lists;
        w
    }

    pub fn from_words(w: &[u32; STATE_WORDS]) -> Self {
        let mut heads = [NONE; CLASSES];
        heads.copy_from_slice(&w[1..1 + CLASSES]);
        ArenaState {
            top: w[0],
            heads,
            live_blocks: w[1 + CLASSES],
            live_bytes: w[2 + CLASSES],
            allocs: w[3 + CLASSES],
            frees: w[4 + CLASSES],
            broken_lists: w[5 + CLASSES],
        }
    }
}

pub const STATE_WORDS: usize = CLASSES + 6;

#[cfg(test)]
mod tests {
    use super::*;

    struct VecMem(Vec<u32>);

    impl VecMem {
        fn new(bytes: u32) -> Self {
            VecMem(vec![0; (bytes / 4) as usize])
        }
    }

    impl ArenaMem for VecMem {
        fn len(&self) -> u32 {
            self.0.len() as u32 * 4
        }
        fn read(&self, off: u32) -> u32 {
            self.0[(off / 4) as usize]
        }
        fn write(&mut self, off: u32, v: u32) {
            self.0[(off / 4) as usize] = v;
        }
    }

    #[test]
    fn block_at_walks_used_and_free_blocks() {
        let mut m = VecMem::new(1 << 16);
        let mut s = ArenaState::new();
        let a = s.alloc_tagged(&mut m, 676, 0xAAAA).unwrap();
        let b = s.alloc_tagged(&mut m, 40, 0xBBBB).unwrap();
        s.free(&mut m, a).unwrap();
        let x = s.block_at(&m, 0).unwrap();
        assert_eq!(
            (x.at, x.payload_bytes, x.used, x.size, x.tag),
            (0, 1024, false, 676, None)
        );
        let y = s.block_at(&m, HEADER + x.payload_bytes).unwrap();
        assert_eq!(y.at, b - HEADER);
        assert_eq!(
            (y.payload_bytes, y.used, y.size, y.tag),
            (64, true, 40, Some(0xBBBB))
        );
        assert_eq!(s.block_at(&m, y.at + HEADER + y.payload_bytes), None);
        assert_eq!(s.block_at(&m, 4), None, "先頭でない位置");
        m.write(HEADER + x.payload_bytes + 4, 99);
        assert_eq!(s.block_at(&m, y.at), None, "級が壊れた頭");
    }

    #[test]
    fn free_zeroes_the_whole_payload_and_nothing_else() {
        let mut m = VecMem::new(1 << 14);
        let mut s = ArenaState::new();
        let before = s.alloc_tagged(&mut m, 100, 0x11).unwrap();
        let a = s.alloc_tagged(&mut m, 676, 0xAA).unwrap(); // 級 1024
        let after = s.alloc_tagged(&mut m, 100, 0x22).unwrap();
        for (p, n) in [(before, 128u32), (a, 1024), (after, 128)] {
            for off in (p..p + n).step_by(4) {
                m.write(off, 0xDEAD_BEEF);
            }
        }
        let head = |m: &VecMem, p: u32| {
            (0..4)
                .map(|i| m.read(p - HEADER + i * 4))
                .collect::<Vec<_>>()
        };
        let (head_before, head_after) = (head(&m, before), head(&m, after));
        s.free(&mut m, a).unwrap();
        for off in (a..a + 1024).step_by(4) {
            assert_eq!(m.read(off), 0, "解放した中身の +{} が残っている", off - a);
        }
        // ヘッダ: 印は空き、級と求められた大きさは残る、4 語目は列の次。
        assert_eq!(
            head(&m, a),
            vec![MAGIC_FREE, class_of(676).unwrap() as u32, 676, NONE]
        );
        assert_eq!(head(&m, before), head_before);
        assert_eq!(head(&m, after), head_after);
        for (p, n) in [(before, 128u32), (after, 128)] {
            for off in (p..p + n).step_by(4) {
                assert_eq!(m.read(off), 0xDEAD_BEEF, "隣の塊の +{} を書いた", off - p);
            }
        }
    }

    #[test]
    fn a_reused_block_shows_nothing_of_the_previous_owner() {
        let mut m = VecMem::new(1 << 14);
        let mut s = ArenaState::new();
        let a = s.alloc(&mut m, 1000).unwrap(); // 級 1024
        for off in (a..a + 1024).step_by(4) {
            m.write(off, 0x1234_5678);
        }
        s.free(&mut m, a).unwrap();
        // 同じ級で小さい大きさを求めると、同じ塊が使い回されて全部の語が 0。
        let b = s.alloc(&mut m, 513).unwrap();
        assert_eq!(b, a, "前提: 同じ塊を使い回す");
        for off in (b..b + 1024).step_by(4) {
            assert_eq!(
                m.read(off),
                0,
                "再確保した塊の +{} に前の中身が見える",
                off - b
            );
        }
        m.write(b + 900, 7);
        s.free(&mut m, b).unwrap();
        let c = s.alloc(&mut m, 600).unwrap();
        assert_eq!((c, m.read(c + 900)), (a, 0));
    }

    #[test]
    fn zeroing_keeps_restore_and_replay_identical() {
        let mut m = VecMem::new(1 << 16);
        let mut s = ArenaState::new();
        let mut live = Vec::new();
        for i in 0..12u32 {
            let p = s.alloc(&mut m, 90 + i * 53).unwrap();
            m.write(p, 0xC0DE_0000 + i);
            live.push(p);
        }
        for p in live.drain(..6) {
            s.free(&mut m, p).unwrap();
        }
        let snap_state = s;
        let snap_mem: Vec<u32> = m.0[..(s.top / 4) as usize].to_vec();
        let run = |s: &mut ArenaState, m: &mut VecMem, live: &mut Vec<u32>| {
            for i in 0..20u32 {
                if i % 3 == 2 {
                    if let Some(p) = live.pop() {
                        s.free(m, p).unwrap();
                    }
                } else {
                    let p = s.alloc(m, 40 + i * 71).unwrap();
                    m.write(p, i + 1);
                    live.push(p);
                }
            }
        };
        let mut live1 = live.clone();
        run(&mut s, &mut m, &mut live1);
        let after1 = (s, m.0.clone());
        let top_now = s.top;
        s = snap_state;
        m.0[..snap_mem.len()].copy_from_slice(&snap_mem);
        for w in &mut m.0[(snap_state.top / 4) as usize..(top_now / 4) as usize] {
            *w = 0;
        }
        let mut live2 = live.clone();
        run(&mut s, &mut m, &mut live2);
        assert_eq!((s, m.0.clone()), after1);
    }

    #[test]
    fn classes_round_up_to_powers_of_two() {
        assert_eq!(class_of(0), Some(0));
        assert_eq!(class_of(1), Some(0));
        assert_eq!(class_of(16), Some(0));
        assert_eq!(class_of(17), Some(1));
        assert_eq!(class_of(676), Some(6)); // エフェクトの VM の配列 1 本（0x2A4）は 1024 の級
        assert_eq!(class_bytes(6), 1024);
        assert_eq!(class_of(0x8000_0000), Some(27));
        assert_eq!(class_of(0x8000_0001), None);
    }

    #[test]
    fn blocks_are_aligned_and_do_not_overlap() {
        let mut m = VecMem::new(1 << 16);
        let mut s = ArenaState::new();
        let sizes = [1u32, 16, 17, 100, 676, 3000, 0];
        let mut got = Vec::new();
        for &n in &sizes {
            let p = s.alloc(&mut m, n).expect("入る");
            assert_eq!(p % ALIGN, 0);
            got.push((p, class_bytes(class_of(n).unwrap())));
        }
        for (i, &(p, n)) in got.iter().enumerate() {
            for &(q, _) in &got[i + 1..] {
                assert!(q - HEADER >= p + n, "{p} と {q} が重なる");
            }
        }
        for &(p, n) in &got {
            for off in (p..p + n).step_by(4) {
                m.write(off, 0xDEAD_BEEF);
            }
        }
        for &(p, _) in &got {
            assert_eq!(s.free(&mut m, p), Ok(()));
        }
        assert_eq!((s.live_blocks, s.live_bytes), (0, 0));
    }

    #[test]
    fn freed_blocks_are_reused_last_in_first_out() {
        let mut m = VecMem::new(1 << 16);
        let mut s = ArenaState::new();
        let a = s.alloc(&mut m, 600).unwrap();
        let b = s.alloc(&mut m, 700).unwrap();
        let small = s.alloc(&mut m, 8).unwrap();
        let top = s.top;
        s.free(&mut m, a).unwrap();
        s.free(&mut m, b).unwrap();
        let c = s.alloc(&mut m, 8).unwrap();
        assert!(c >= top, "小さい級が 1024 の級の列から取った");
        let top = s.top;
        assert_eq!(s.alloc(&mut m, 1000), Some(b));
        assert_eq!(s.alloc(&mut m, 513), Some(a));
        assert_eq!(s.top, top, "使い回せるのに上へ伸ばした");
        assert_ne!(small, c);
    }

    #[test]
    fn bad_frees_are_refused_without_writing() {
        let mut m = VecMem::new(1 << 12);
        let mut s = ArenaState::new();
        let a = s.alloc(&mut m, 32).unwrap();
        s.free(&mut m, a).unwrap();
        let before = (s, m.0.clone());
        assert_eq!(s.free(&mut m, a), Err(FreeError::NotInUse));
        assert_eq!(s.free(&mut m, a + 4), Err(FreeError::Misaligned));
        assert_eq!(s.free(&mut m, a + 16), Err(FreeError::NotInUse));
        assert_eq!(s.free(&mut m, 0), Err(FreeError::Misaligned));
        assert_eq!(s.free(&mut m, s.top), Err(FreeError::OutOfRange));
        assert_eq!(s.free(&mut m, 1 << 20), Err(FreeError::OutOfRange));
        assert_eq!((s, m.0.clone()), before, "断ったのに書いた");
        let b = s.alloc(&mut m, 32).unwrap();
        assert_eq!(b, a);
        m.write(b - HEADER + 4, 40);
        m.write(b, 0x7777);
        let mem_before = m.0.clone();
        assert_eq!(s.free(&mut m, b), Err(FreeError::BadHeader));
        assert_eq!(m.0, mem_before, "断った解放が中身を 0 にした");
        let c = s.alloc(&mut m, 32).unwrap();
        assert_ne!(c, b);
    }

    #[test]
    fn exhaustion_returns_none_and_changes_nothing() {
        let mut m = VecMem::new(256);
        let mut s = ArenaState::new();
        assert!(s.alloc(&mut m, 100).is_some()); // 16 + 128
        let before = s;
        assert_eq!(s.alloc(&mut m, 100), None);
        assert_eq!(s, before);
        assert!(s.alloc(&mut m, 90).is_none());
        assert!(s.alloc(&mut m, 64).is_some()); // 16 + 64 = 80 で、残り 112 に入る
        assert!(s.top <= 256);
        assert_eq!(s.alloc(&mut m, u32::MAX), None);
    }

    #[test]
    fn a_broken_free_list_is_dropped() {
        let mut m = VecMem::new(1 << 12);
        let mut s = ArenaState::new();
        let a = s.alloc(&mut m, 32).unwrap();
        s.free(&mut m, a).unwrap();
        m.write(a - HEADER, 0x1234_5678);
        let b = s.alloc(&mut m, 32).unwrap();
        assert_ne!(b, a);
        assert_eq!(s.broken_lists, 1);
        assert_eq!(s.heads[class_of(32).unwrap()], NONE);
    }

    #[test]
    fn restoring_state_and_prefix_replays_identically() {
        let mut m = VecMem::new(1 << 16);
        let mut s = ArenaState::new();
        let mut live = Vec::new();
        for i in 0..20u32 {
            live.push(s.alloc(&mut m, 100 + i * 37).unwrap());
        }
        for p in live.drain(..10) {
            s.free(&mut m, p).unwrap();
        }
        let snap_state = s;
        let snap_mem: Vec<u32> = m.0[..(s.top / 4) as usize].to_vec();
        let run = |s: &mut ArenaState, m: &mut VecMem, live: &mut Vec<u32>| -> Vec<u32> {
            let mut out = Vec::new();
            for i in 0..30u32 {
                if i % 3 == 2 {
                    if let Some(p) = live.pop() {
                        s.free(m, p).unwrap();
                    }
                } else {
                    let p = s.alloc(m, 50 + i * 91).unwrap();
                    m.write(p, i);
                    live.push(p);
                    out.push(p);
                }
            }
            out
        };
        let mut live1 = live.clone();
        let first = run(&mut s, &mut m, &mut live1);
        let after1 = (s, m.0.clone());
        let top_now = s.top;
        s = snap_state;
        m.0[..snap_mem.len()].copy_from_slice(&snap_mem);
        for w in &mut m.0[(snap_state.top / 4) as usize..(top_now / 4) as usize] {
            *w = 0;
        }
        let mut live2 = live.clone();
        let second = run(&mut s, &mut m, &mut live2);
        assert_eq!(first, second, "進め直しで違う位置を配った");
        assert_eq!((s, m.0.clone()), after1, "進め直しの後の状態か中身が違う");
    }

    #[test]
    fn used_blocks_are_walked_with_their_tags() {
        let mut m = VecMem::new(1 << 12);
        let mut s = ArenaState::new();
        let a = s.alloc_tagged(&mut m, 0x79, 0x0042_03F2).unwrap();
        let b = s.alloc(&mut m, 20).unwrap();
        let c = s.alloc_tagged(&mut m, 300, 7).unwrap();
        let walk = |s: &ArenaState, m: &VecMem| {
            let mut v = Vec::new();
            let ok = s.for_each_used(m, |p, n, t| v.push((p, n, t)));
            (ok, v)
        };
        assert_eq!(
            walk(&s, &m),
            (
                true,
                vec![(a, 0x79, 0x0042_03F2), (b, 20, NONE), (c, 300, 7)]
            )
        );
        s.free(&mut m, a).unwrap();
        assert_eq!(walk(&s, &m), (true, vec![(b, 20, NONE), (c, 300, 7)]));
        assert_eq!(s.alloc_tagged(&mut m, 100, 9), Some(a));
        assert_eq!(
            walk(&s, &m),
            (true, vec![(a, 100, 9), (b, 20, NONE), (c, 300, 7)])
        );
        m.write(b - HEADER, 0x1234_5678);
        assert_eq!(walk(&s, &m), (false, vec![(a, 100, 9)]));
        assert_eq!(walk(&ArenaState::new(), &m), (true, vec![]));
    }

    #[test]
    fn to_words_covers_every_field() {
        let mut s = ArenaState::new();
        let base = s.to_words();
        let tweaks: [fn(&mut ArenaState); 7] = [
            |s| s.top = 16,
            |s| s.heads[CLASSES - 1] = 0,
            |s| s.live_blocks = 1,
            |s| s.live_bytes = 1,
            |s| s.allocs = 1,
            |s| s.frees = 1,
            |s| s.broken_lists = 1,
        ];
        for t in tweaks {
            let mut x = s;
            t(&mut x);
            assert_ne!(x.to_words(), base);
            assert_eq!(ArenaState::from_words(&x.to_words()), x);
        }
        s.top = 0;
        assert_eq!(s.to_words(), base);
    }
}
