//! 検査だけの偽物: 本物の花映塚の代わりに `rollback_play::PlayMem` を実装する偽のゲーム（`FakeGame`）。
//! 語のアドレス → 値の表で、コピーは範囲の中の語だけをコピーする。`rollback_play` と `netplay/rollback.rs` の検査が使う。
#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use higanr_core::render_reset::{self, Field as ResetField, Refused};

use crate::rollback_play::{Order, OutCounts, PlayMem, PlayState, SUP_FIELDS, ScanRun};
use crate::rollback_probe::{Layout, Region, RegionKind, Tables};
use crate::th09_addr::{
    ASCII_COUNT, EFFECT_ARRAY_COUNT, EFFECT_ARRAY_EXTRA, EFFECT_ARRAY_PTR, FPS_BASE_MS, FPS_DRAWS,
    FPS_SHOWN, RNG_ADDR, RNG_COUNT, ROLLBACK_BLOCKS, ROLLBACK_EFFECT_ARRAYS, SND_BGM_QUEUE,
    SND_SE_COUNTS, SND_SE_IDS, SND_SE_MAX_COUNT, SND_SE_PAN_SLOT_BYTES, SND_SE_PANS, SND_SE_SLOTS,
    SOUND_MANAGER, STAGE_BG_VM_COUNT, STAGE_BG_VMS, STAGE_BLOCKS, STAGE_TINT, STAGE_TINT_AFTER,
    STAGE_TINT_MODE, SUPERVISOR, UPDATE_REGISTRY, UPDATE_REGISTRY_BYTES, VM_BYTES, VM_CLOCK,
    VM_CUR_INSTR,
};

/// 偽の DLL の領域（アドレスは検査の中だけの値。偽のゲームは領域の中身を語の表で持つ）。
pub(crate) const ARENA_BASE: u32 = 0x2000_0000;
pub(crate) const ARENA_BYTES: u32 = 1 << 20;
pub(crate) const STATE: u32 = 0x004A_8000;
/// 列の並びに入るがコピーの範囲の外にある語（Supervisor の中。コピーの外の節のつなぎの代わり）。
pub(crate) const LIST_OUTSIDE: u32 = SUPERVISOR + 0x10;

pub(crate) struct FakeGame {
    pub(crate) mem: BTreeMap<u32, u32>,
    pub(crate) snap: Option<(BTreeMap<u32, u32>, u32)>,
    pub(crate) arena_top: u32,
    pub(crate) redirect: bool,
    pub(crate) redirect_on_calls: u32,
    pub(crate) watch: bool,
    pub(crate) watch_on_calls: u32,
    pub(crate) copy_outs: u32,
    pub(crate) copy_ins: u32,
    /// 走査を呼んだ順（指図・進め直しの回か）。
    pub(crate) scans: Vec<(Order, bool)>,
    pub(crate) writes: Vec<(u32, u32)>,
    pub(crate) counts: OutCounts,
    pub(crate) born: u32,
    pub(crate) now: u64,
    pub(crate) ret: i32,
    pub(crate) consume: bool,
    /// p2 がこの値の tick で次の場面の要求を立てる・列の並びを変える・外の塊を取る。
    pub(crate) scene_on_p2: Option<u16>,
    /// 数えた tick（`RNG_COUNT`。進め直しで戻る）がこの数の倍数になる tick で次の場面の要求を立てる（両者で同じ tick になる）。
    pub(crate) scene_every_ticks: Option<u32>,
    pub(crate) list_on_p2: Option<u16>,
    pub(crate) alloc_on_p2: Option<u16>,
    pub(crate) se_in_resim: u32,
    /// 列の並びを本物と同じ形（レジストリから次をたどる）で読む。偽なら簡単な形。
    pub(crate) real_lists: bool,
    /// 解放を CRT へ渡さなかった塊・旗・節を書き戻した数・節の書き戻しを断るか。
    pub(crate) held: Vec<u32>,
    pub(crate) hold_frees: bool,
    pub(crate) node_writes: u32,
    pub(crate) refuse_node_writes: bool,
    pub(crate) hold_in_scans: Vec<bool>,
    pub(crate) pending_sets: u32,
    /// コピーのときにバックアップした ScreenEffect の本体のアドレス・見張りの外で CRT へ解放が渡った本体・本体を書き戻した数・
    /// 本体の書き戻しを断るか。
    pub(crate) stray_bodies: Vec<u32>,
    pub(crate) freed_bodies: Vec<u32>,
    pub(crate) body_writes: u32,
    pub(crate) refuse_body_writes: bool,
    pub(crate) body_reads: u32,
    /// Stage の色の模様の真似（`th09_addr::STAGE_TINT` の注の本物の形）: p2 がこの値の tick でスペルカードの始まりの形
    /// （1P の Stage に模様 1・2P に模様 2、残り `TINT_TICKS`）を立てる。更新（feed の走査）は模様が 0 でない Stage の色を書き、
    /// 残りが尽きたら模様を 0 にする。描画は色を描画の後の値に戻す。`None` なら Stage の欄に触らない。
    pub(crate) tint_on_p2: Option<u16>,
    pub(crate) reset_calls: u32,
    /// fps の計測 `0x4316B0` の真似: 描画のたびに描画の回数を 1 足し、基準の時刻から 500 ms 以上なら fps を計算し直して
    /// 丸めた値を `FPS_SHOWN` に置く。時刻は描画の回数 × 50 / 3 ms（60 fps）。偽なら fps の欄に触らない。
    pub(crate) fps_model: bool,
    pub(crate) draws: u32,
    /// 毎 tick に `s & 0x3F` の id の SE を積む雑音（既定 true）。SE の検査は false にして、決まった出来事だけを積む。
    pub(crate) se_noise: bool,
    /// 相手の入力ワード（p2）が前の tick から変わった tick で `id + (p2 & 0xF)` の SE を積む（状態は `STATE + 8` に持つので書き戻しで戻る）・
    /// 自分の入力ワード（p1）が変わった tick で同じく（`STATE + 4`）。予測は相手の直前の入力ワードの繰り返しなので、相手の変わり目の出来事は
    /// 予測では起きず、進め直しの確定の回で初めて起きる。
    pub(crate) se_p2_change: Option<u32>,
    pub(crate) se_p1_change: Option<u32>,
    /// p2 がこの値の tick で SE を積む（状態を持たない）。
    pub(crate) se_p2_eq: Option<(u16, u32)>,
    /// `push_se`（遅れて積み直し）で積んだ (id, パン) の列。`se_refuse` が真なら先頭のバイト列が合わない本物と同じに断る。
    pub(crate) se_pushed: Vec<(u32, i32)>,
    pub(crate) se_refuse: bool,
    /// 鳴らした（`play_and_draw` が待ち行列を空にした）SE の記録: (何回目の do_frame か, id, 回数)。
    pub(crate) se_log: Vec<(u32, u32, u32)>,
    pub(crate) se_draws: u32,
    /// 読めないアドレス（`read_u32` が `None` を返す。出力のバックアップを取れなかった進め直しの形）。
    pub(crate) deny_read: Option<u32>,
}

/// 偽のゲームの陣の背景の VM の配列のアドレス（陣ごと。検査の中だけの値）。
pub(crate) const BG_ARRAY: [u32; 2] = [0x0E00_0000, 0x0E10_0000];

/// 偽のゲームのスペルカードの模様の長さ（本物 `0x404161 mov $0x1e`）。
pub(crate) const TINT_TICKS: u32 = 0x1E;

impl FakeGame {
    pub(crate) fn new() -> Self {
        let mut mem = BTreeMap::new();
        for (i, b) in ROLLBACK_BLOCKS.iter().enumerate() {
            mem.insert(b.ptr_at, 0x1000_0000 + i as u32 * 0x0040_0000);
        }
        for (i, e) in ROLLBACK_EFFECT_ARRAYS.iter().enumerate() {
            let sys = mem[&e.sys_ptr_at];
            mem.insert(sys + EFFECT_ARRAY_PTR, 0x0C00_0000 + i as u32 * 0x0010_0000);
            mem.insert(sys + EFFECT_ARRAY_COUNT, e.count);
            mem.insert(sys + EFFECT_ARRAY_EXTRA, e.extra);
        }
        mem.insert(SUP_FIELDS[0], 2);
        mem.insert(SUP_FIELDS[1], 2);
        for i in 0..12 {
            mem.insert(SOUND_MANAGER + SND_SE_IDS + i * 4, u32::MAX);
            mem.insert(SOUND_MANAGER + SND_SE_COUNTS + i * 4, 0);
        }
        mem.insert(SOUND_MANAGER + SND_BGM_QUEUE, 0);
        mem.insert(ASCII_COUNT, 0);
        mem.insert(STATE, 1);
        mem.insert(RNG_ADDR, 0x1234);
        mem.insert(RNG_COUNT, 0);
        for off in (0..UPDATE_REGISTRY_BYTES).step_by(4) {
            mem.insert(UPDATE_REGISTRY + off, 0xA000 + off);
        }
        mem.insert(LIST_OUTSIDE, 7);
        FakeGame {
            mem,
            snap: None,
            arena_top: 0,
            redirect: false,
            redirect_on_calls: 0,
            watch: false,
            watch_on_calls: 0,
            copy_outs: 0,
            copy_ins: 0,
            scans: Vec::new(),
            writes: Vec::new(),
            counts: OutCounts::default(),
            born: 0,
            now: 0,
            ret: 30,
            consume: true,
            scene_on_p2: None,
            scene_every_ticks: None,
            list_on_p2: None,
            alloc_on_p2: None,
            se_in_resim: 0,
            real_lists: false,
            held: Vec::new(),
            hold_frees: false,
            node_writes: 0,
            refuse_node_writes: false,
            hold_in_scans: Vec::new(),
            pending_sets: 0,
            stray_bodies: Vec::new(),
            freed_bodies: Vec::new(),
            body_writes: 0,
            refuse_body_writes: false,
            body_reads: 0,
            tint_on_p2: None,
            reset_calls: 0,
            fps_model: false,
            draws: 0,
            se_noise: true,
            se_p2_change: None,
            se_p1_change: None,
            se_p2_eq: None,
            se_pushed: Vec::new(),
            se_refuse: false,
            se_log: Vec::new(),
            se_draws: 0,
            deny_read: None,
        }
    }

    /// SE を 1 つ積む（本物の `0x43E2F0` の形: 同じ id の枠があれば回数を増やしてパンを足し、無ければ最初の空き枠へ id・パン・回数 1。
    /// 回数が 0x80 以上なら足さない。12 枠が埋まっていれば捨てる）。
    pub(crate) fn se_queue_push(&mut self, id: u32, pan: i32) {
        for slot in 0..SND_SE_SLOTS {
            let id_at = SOUND_MANAGER + SND_SE_IDS + slot * 4;
            let count_at = SOUND_MANAGER + SND_SE_COUNTS + slot * 4;
            let cur = self.get(id_at);
            if (cur as i32) < 0 {
                self.mem.insert(id_at, id);
                self.mem.insert(
                    SOUND_MANAGER + SND_SE_PANS + slot * SND_SE_PAN_SLOT_BYTES,
                    pan as u32,
                );
                self.mem.insert(count_at, 1);
                return;
            }
            if cur == id {
                let c = self.get(count_at);
                if c < SND_SE_MAX_COUNT {
                    self.mem.insert(
                        SOUND_MANAGER + SND_SE_PANS + slot * SND_SE_PAN_SLOT_BYTES + c * 4,
                        pan as u32,
                    );
                    self.mem.insert(count_at, c + 1);
                }
                return;
            }
        }
    }

    fn se_events(&mut self, p1: u16, p2: u16) {
        if let Some(base) = self.se_p2_change {
            let prev = self.get(STATE + 8);
            if u32::from(p2) != prev {
                self.se_queue_push(base + u32::from(p2 & 0xF), 0);
            }
            self.mem.insert(STATE + 8, u32::from(p2));
        }
        if let Some(base) = self.se_p1_change {
            let prev = self.get(STATE + 4);
            if u32::from(p1) != prev {
                self.se_queue_push(base + u32::from(p1 & 0xF), 0);
            }
            self.mem.insert(STATE + 4, u32::from(p1));
        }
        if let Some((word, id)) = self.se_p2_eq
            && p2 == word
        {
            self.se_queue_push(id, 0);
        }
    }

    pub(crate) fn se_queue(&self) -> Vec<(u32, u32)> {
        (0..SND_SE_SLOTS)
            .map(|s| {
                (
                    self.get(SOUND_MANAGER + SND_SE_IDS + s * 4),
                    self.get(SOUND_MANAGER + SND_SE_COUNTS + s * 4),
                )
            })
            .take_while(|(id, _)| (*id as i32) >= 0)
            .collect()
    }

    pub(crate) fn played_ids(&self) -> Vec<u32> {
        self.se_log.iter().map(|&(_, id, _)| id).collect()
    }

    /// 両陣の Stage に陣の背景の VM の配列（`n` 本。どれも動いている —— 今の命令が 0 でない）を置く（本物は読み込みが置く）。
    pub(crate) fn set_bg(&mut self, n: u32) {
        for (side, &arr) in BG_ARRAY.iter().enumerate() {
            let s = self.stage(side);
            self.mem.insert(s + STAGE_BG_VMS, arr);
            self.mem.insert(s + STAGE_BG_VM_COUNT, n);
            for i in 0..n {
                let vm = arr + i * VM_BYTES;
                self.mem.insert(vm + VM_CUR_INSTR, 0x0070_0000 + i);
                self.mem.insert(vm + VM_CLOCK, 0);
            }
        }
    }

    pub(crate) fn bg_clock(&self, side: usize, i: u32) -> u32 {
        self.get(BG_ARRAY[side] + i * VM_BYTES + VM_CLOCK)
    }

    /// 更新 `Stage::on_tick 0x402350` → `0x4011D0` の真似（feed の走査の中）: 置いた VM の時計を 1 進める（run_vm の尻
    /// `0x4393A1`）。本数は Stage の欄から読む（コピーの書き戻しで戻った値で回る）。
    fn bg_tick(&mut self) {
        for side in 0..2 {
            let s = self.stage(side);
            let (arr, n) = (self.get(s + STAGE_BG_VMS), self.get(s + STAGE_BG_VM_COUNT));
            if arr == 0 {
                continue;
            }
            for i in 0..n {
                let vm = arr + i * VM_BYTES;
                if self.get(vm + VM_CUR_INSTR) != 0 {
                    self.mem.insert(vm + VM_CLOCK, self.get(vm + VM_CLOCK) + 1);
                }
            }
        }
    }

    /// fps の計測 `0x4316B0` の真似（描画の中）。
    fn fps_draw(&mut self) {
        self.draws += 1;
        let now = self.draws * 50 / 3;
        let count = self.get(FPS_DRAWS) + 1;
        self.mem.insert(FPS_DRAWS, count);
        let base = self.get(FPS_BASE_MS);
        if now.wrapping_sub(base) >= 500 {
            // `0x43171D cmp $0x1f4` → 回数 ÷ 経った秒、+0.5 を切り捨て（`0x4318B8` / `0x4318BE`）。
            let fps = (count * 1000 + (now - base) / 2) / (now - base);
            self.mem.insert(FPS_BASE_MS, now);
            self.mem.insert(FPS_DRAWS, 0);
            self.mem.insert(FPS_SHOWN, fps & 0xFFFF);
        }
    }

    pub(crate) fn get(&self, a: u32) -> u32 {
        self.mem.get(&a).copied().unwrap_or(0)
    }

    pub(crate) fn stage(&self, side: usize) -> u32 {
        let ptr_at = ROLLBACK_BLOCKS
            .iter()
            .find(|b| b.name == STAGE_BLOCKS[side])
            .map(|b| b.ptr_at)
            .expect("Stage の塊が表にある");
        self.get(ptr_at)
    }

    /// do_frame が走査の後に SE と BGM を鳴らして空にし、描画で文字の数を 0 にする形。Stage の色の真似が入っていれば、
    /// 描画 `0x403680` と同じく色を描画の後の値に戻す。
    pub(crate) fn play_and_draw(&mut self) {
        self.se_draws += 1;
        let n = self.se_draws;
        let queued = self.se_queue();
        self.se_log
            .extend(queued.into_iter().map(|(id, c)| (n, id, c)));
        for i in 0..12 {
            self.mem
                .insert(SOUND_MANAGER + SND_SE_IDS + i * 4, u32::MAX);
            self.mem.insert(SOUND_MANAGER + SND_SE_COUNTS + i * 4, 0);
        }
        self.mem.insert(ASCII_COUNT, 0);
        if self.fps_model {
            self.fps_draw();
        }
        if self.tint_on_p2.is_some() {
            for side in 0..2 {
                let s = self.stage(side);
                self.mem
                    .insert(s + STAGE_TINT, u32::from_le_bytes(STAGE_TINT_AFTER));
            }
        }
    }

    /// 更新 `Stage::on_tick 0x402350` の色の部分の真似。`p2` が `tint_on_p2` なら模様を立てる（スペルカードの始まり
    /// `0x403E50` の真似。更新の中で立つので、その tick の Stage の更新より前に立てる。本物の順は読んでいない）。
    fn tint_tick(&mut self, p2: u16) {
        let Some(trigger) = self.tint_on_p2 else {
            return;
        };
        for side in 0..2 {
            let s = self.stage(side);
            if p2 == trigger {
                self.mem.insert(s + STAGE_TINT_MODE, side as u32 + 1);
                self.mem.insert(s + STAGE_TINT_MODE + 4, TINT_TICKS);
            }
            let mode = self.get(s + STAGE_TINT_MODE);
            if mode == 0 {
                continue;
            }
            let left = self.get(s + STAGE_TINT_MODE + 4).saturating_sub(1);
            self.mem.insert(s + STAGE_TINT_MODE + 4, left);
            if left == 0 {
                self.mem.insert(s + STAGE_TINT_MODE, 0);
                continue;
            }
            let tint = if mode == 1 { 0x80F0_0000 } else { 0x8030_3040 };
            self.mem.insert(s + STAGE_TINT, tint);
        }
    }

    pub(crate) fn outputs(&self) -> Vec<u32> {
        let mut v = Vec::new();
        for i in 0..12 {
            v.push(self.get(SOUND_MANAGER + SND_SE_IDS + i * 4));
            v.push(self.get(SOUND_MANAGER + SND_SE_COUNTS + i * 4));
        }
        v.push(self.get(SOUND_MANAGER + SND_BGM_QUEUE));
        v.push(self.get(ASCII_COUNT));
        v
    }

    pub(crate) fn registry(&self) -> Vec<u32> {
        (0..UPDATE_REGISTRY_BYTES)
            .step_by(4)
            .map(|off| self.get(UPDATE_REGISTRY + off))
            .collect()
    }

    pub(crate) fn in_regions(l: &Layout, a: u32) -> bool {
        l.regions().iter().any(|r| {
            r.kind != RegionKind::ArenaState
                && (r.addr..r.addr.saturating_add(r.len.min(r.cap))).contains(&a)
        })
    }
}

impl PlayMem for FakeGame {
    fn read_u32(&mut self, addr: u32) -> Option<u32> {
        (self.deny_read != Some(addr)).then(|| self.get(addr))
    }
    fn write_u32(&mut self, addr: u32, v: u32) -> bool {
        self.writes.push((addr, v));
        self.mem.insert(addr, v);
        true
    }
    fn check_rw(&mut self, _addr: u32, _len: u32) -> bool {
        true
    }
    fn arena_top(&mut self) -> u32 {
        self.arena_top
    }
    fn copy_out(&mut self, l: &Layout) {
        self.copy_outs += 1;
        let s: BTreeMap<u32, u32> = self
            .mem
            .iter()
            .filter(|(a, _)| Self::in_regions(l, **a))
            .map(|(a, v)| (*a, *v))
            .collect();
        self.snap = Some((s, self.arena_top));
    }
    fn copy_in(&mut self, l: &Layout) {
        self.copy_ins += 1;
        let Some((s, top)) = self.snap.clone() else {
            return;
        };
        self.mem.retain(|a, _| !Self::in_regions(l, *a));
        self.mem.extend(s);
        self.arena_top = top;
    }
    /// コピー（語の表）の範囲 `r` の中を塊のバイト列に組み直して本物と同じ `render_reset::apply` を掛け、欄に掛かる語だけを
    /// コピーへ戻す（偽のゲームの語は 4 の倍数のアドレスに置く）。
    fn reset_in_snap(&mut self, r: &Region, fields: &[ResetField<'_>]) -> Result<u32, Refused> {
        self.reset_calls += 1;
        let len = r.len.min(r.cap);
        let Some((s, _)) = self.snap.as_mut() else {
            return Ok(0);
        };
        let mut block = vec![0u8; len as usize];
        for (&a, &v) in s.range(r.addr..r.addr.saturating_add(len)) {
            for (k, b) in v.to_le_bytes().into_iter().enumerate() {
                if let Some(x) = block.get_mut((a - r.addr) as usize + k) {
                    *x = b;
                }
            }
        }
        let n = render_reset::apply(&mut block, fields)?;
        for f in fields {
            let end = f.off + f.after.len() as u32;
            let mut w = f.off & !3;
            while w < end {
                let mut v = [0u8; 4];
                for (k, x) in v.iter_mut().enumerate() {
                    *x = block.get(w as usize + k).copied().unwrap_or(0);
                }
                s.insert(r.addr + w, u32::from_le_bytes(v));
                w += 4;
            }
        }
        Ok(n)
    }
    fn list_signature(&mut self) -> u64 {
        if self.real_lists {
            let m = &self.mem;
            return crate::rollback_play::list_signature_with(&mut |a| {
                Some(m.get(&a).copied().unwrap_or(0))
            });
        }
        let mut h = 0xCBF2_9CE4_8422_2325u64;
        for w in self.registry().into_iter().chain([self.get(LIST_OUTSIDE)]) {
            h = (h ^ u64::from(w)).wrapping_mul(0x100_0000_01B3);
        }
        h
    }
    fn run_scan(&mut self, order: Order, resim: bool) -> ScanRun {
        self.scans.push((order, resim));
        self.hold_in_scans.push(self.hold_frees);
        if let Order::Feed { p1, p2 } = order {
            let s = self
                .get(STATE)
                .wrapping_mul(31)
                .wrapping_add(u32::from(p1) * 7 + u32::from(p2) + 1);
            self.mem.insert(STATE, s);
            self.mem.insert(RNG_ADDR, s & 0xFFFF);
            self.mem.insert(RNG_COUNT, self.get(RNG_COUNT) + 1);
            // SE・BGM・文字の数を積む（本物は走査の中の SE の積み `0x43E2F0`・BGM の `0x43EE40`・文字の `0x4342A0`）。
            if self.se_noise {
                self.se_queue_push(s & 0x3F, 0);
            }
            self.se_events(p1, p2);
            self.mem.insert(
                SOUND_MANAGER + SND_BGM_QUEUE,
                self.get(SOUND_MANAGER + SND_BGM_QUEUE) + 1,
            );
            self.mem.insert(ASCII_COUNT, self.get(ASCII_COUNT) + 1);
            if resim {
                self.se_in_resim += 1;
            }
            if self.scene_on_p2 == Some(p2)
                || self
                    .scene_every_ticks
                    .is_some_and(|n| self.get(RNG_COUNT).is_multiple_of(n))
            {
                self.mem.insert(SUP_FIELDS[1], self.get(SUP_FIELDS[0]) + 1);
            }
            if self.list_on_p2 == Some(p2) {
                self.mem
                    .insert(UPDATE_REGISTRY + 0x14, self.get(UPDATE_REGISTRY + 0x14) + 1);
            }
            if self.alloc_on_p2 == Some(p2) && self.watch {
                self.counts.allocs += 1;
                self.born += 1;
            }
            self.tint_tick(p2);
            self.bg_tick();
            self.arena_top += 16;
        }
        ScanRun {
            ret: if order == Order::RenderOnly {
                1
            } else {
                self.ret
            },
            consumed: self.consume,
        }
    }
    fn now_us(&mut self) -> u64 {
        self.now += 10;
        self.now
    }
    fn set_redirect(&mut self, on: bool) {
        self.redirect = on;
        if on {
            self.redirect_on_calls += 1;
        }
    }
    fn set_watch(&mut self, on: bool) {
        self.watch = on;
        if on {
            self.watch_on_calls += 1;
        }
    }
    fn out_counts(&mut self) -> OutCounts {
        self.counts
    }
    fn born_len(&mut self) -> u32 {
        self.born
    }
    fn clear_born(&mut self) {
        self.born = 0;
        self.held.clear();
    }
    fn list_chain(&mut self, out: &mut crate::rollback_play::ListChain) -> u64 {
        if !self.real_lists {
            *out = crate::rollback_play::ListChain::EMPTY;
            return self.list_signature();
        }
        let m = &self.mem;
        crate::rollback_play::list_chain_with(&mut |a| Some(m.get(&a).copied().unwrap_or(0)), out)
    }
    fn set_pending(&mut self) {
        self.pending_sets += 1;
    }
    fn set_hold_frees(&mut self, on: bool) {
        self.hold_frees = on;
    }
    fn freed_held(&mut self, addr: u32) -> bool {
        self.held.contains(&addr)
    }
    fn write_node(&mut self, addr: u32, w: &[u32; crate::rollback_play::NODE_WORDS]) -> bool {
        if self.refuse_node_writes {
            return false;
        }
        for (i, v) in w.iter().enumerate() {
            self.mem.insert(addr + 4 * i as u32, *v);
        }
        self.node_writes += 1;
        true
    }
    fn set_stray_refs(&mut self, _frame: u32, _nodes: &[u32], _blocks: &[u32], bodies: &[u32]) {
        self.stray_bodies = bodies.to_vec();
        self.freed_bodies.clear();
    }
    fn read_body(&mut self, addr: u32) -> Option<[u32; crate::rollback_play::BODY_WORDS]> {
        self.body_reads += 1;
        let mut w = [0u32; crate::rollback_play::BODY_WORDS];
        for (i, v) in w.iter_mut().enumerate() {
            *v = self.get(addr + 4 * i as u32);
        }
        Some(w)
    }
    fn write_body(&mut self, addr: u32, w: &[u32; crate::rollback_play::BODY_WORDS]) -> bool {
        if self.refuse_body_writes {
            return false;
        }
        for (i, v) in w.iter().enumerate() {
            self.mem.insert(addr + 4 * i as u32, *v);
        }
        self.body_writes += 1;
        true
    }
    /// 本物と同じ向き: バックアップした覚えが無い本体は「解放されていないとは言えない」ので真。
    fn body_freed(&mut self, addr: u32) -> bool {
        !self.stray_bodies.contains(&addr) || self.freed_bodies.contains(&addr)
    }
    /// 本物と同じ向き: 先頭のバイト列が合わなければ（`se_refuse`）積まずに false。積めば待ち行列へ本物の形で積む。
    fn push_se(&mut self, id: u32, pan: i32) -> bool {
        if self.se_refuse {
            return false;
        }
        self.se_pushed.push((id, pan));
        self.se_queue_push(id, pan);
        true
    }
}

pub(crate) fn play_state() -> PlayState {
    PlayState::new(Tables::th09_play(), ARENA_BASE, ARENA_BYTES).expect("表を作れる")
}
