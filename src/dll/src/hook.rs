//! 自分のプロセスのコードを書き換える手順（pure。unsafe 無し）: call 先の差し替え（1 地点）と、inline detour（`detour.rs`）が共に
//! 使う「止める → EIP を見る → 止めたまま読み直す → 書く → 再開する」。Win32（スレッドの一覧・停止・EIP・コードの読み書き・時計）は
//! `HookEnv` の向こうに置く（本物は `hook_win::RealHook`、検査では偽物）。ここは順番と判定だけを持つ。
//!
//! call 先の差し替えの手順:
//! 1. 止める前に: 表の行が `E8 rel32` の形で、rel32 から計算した飛び先が表の元の飛び先と一致するか。いまのバイト列が期待バイト列と
//!    一致するか。一致したら元の飛び先・ウィンドウのアドレス・コピーの表・描画だけのフレームの出口（どれも入口が読む）を入口の静的変数に置く。
//! 2. 試み（最大 `RETRY_LIMIT` 回。`run_attempts`。inline detour も同じ）: スレッドの一覧を取って開く（ヒープはここで使う）→
//!    自分以外を全部止める → 各スレッドの EIP が書き換える命令の全長 `[addr, addr + 長さ)` に無いか（あれば全部再開して
//!    `RETRY_WAIT_MS` 待って次の試み。EIP が取れないスレッドがあれば書かない）→ 止めたまま全長を読み直して期待と一致か →
//!    書く（保護の変更・戻し・命令キャッシュは `HookEnv::write_code` の中）→ 全部再開。call 先の差し替えの全長は 5、書くのは
//!    rel32 の 4 バイトだけ。
//! 3. 再開の後に読み返し、飛び先が入口と一致するかを報告に入れる。
//!
//! 止めている間はヒープを使わない（止めたスレッドがヒープのロックを持っていると自分が止まる）。止めている間の経路は、固定長の配列
//! （`CodeBytes`）と数だけで書き、`format!`・`Vec` の伸長・`unwrap`・範囲外になりうる添字を置かない。止めたスレッドの再開は
//! `Stopped` の `Drop` に置き、パニックでも戻す。
//!
//! 全スレッドを止めて EIP を確かめてから書く作りは、MinHook など一般のフックの道具と同じ仕組みを参考にした（コードはコピーしていない）。
#![forbid(unsafe_code)]

use std::io::Write;

use higanr_core::patch::{self, MAX_PATCH_LEN};

use crate::sites::{SiteDef, Use, hex_bytes};
use crate::ticks::CopyRow;

/// EIP が書き換える範囲にあったときの試みの上限。
pub const RETRY_LIMIT: u32 = 50;
/// EIP が範囲にあったとき、全部再開してから次の試みまで待つ時間。本物は `Sleep` なので、OS のタイマーの間隔（既定で約 15.6 ms）まで延びうる。
pub const RETRY_WAIT_MS: u32 = 2;

/// スレッドのハンドル。中身は数として持つだけで、ここでは参照しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadRef(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteDone {
    pub protect_restored: bool,
    pub flushed: bool,
}

/// コードのバイト列（最大 `MAX_PATCH_LEN` = 16 バイト。書き換える命令の全長・書く中身・読み直した中身）。
/// 固定長の配列なので、止めている間に作ってもヒープを使わない。比べるのは有効な長さの分だけ。
#[derive(Clone, Copy)]
pub struct CodeBytes {
    buf: [u8; MAX_PATCH_LEN],
    len: u8,
}

impl CodeBytes {
    /// `b` をコピーする。`MAX_PATCH_LEN` より長ければ `None`。
    pub fn new(b: &[u8]) -> Option<Self> {
        if b.len() > MAX_PATCH_LEN {
            return None;
        }
        let mut buf = [0u8; MAX_PATCH_LEN];
        buf[..b.len()].copy_from_slice(b);
        Some(CodeBytes {
            buf,
            len: b.len() as u8,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    // clippy の `len_without_is_empty` のため `len` と対で置く（検査でだけ使う読み取り）。
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 先頭の 5 バイト（`E8` / `E9` の rel32 を読み戻すため）。5 バイトに満たなければ `None`。
    pub fn head5(&self) -> Option<[u8; 5]> {
        self.as_slice().get(..5)?.try_into().ok()
    }
}

impl<const N: usize> From<[u8; N]> for CodeBytes {
    fn from(b: [u8; N]) -> Self {
        // 長さは建てるときに決まる（16 を越える配列からは作れない）。
        const { assert!(N <= MAX_PATCH_LEN) };
        let mut buf = [0u8; MAX_PATCH_LEN];
        buf[..N].copy_from_slice(&b);
        CodeBytes { buf, len: N as u8 }
    }
}

impl PartialEq for CodeBytes {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}
impl Eq for CodeBytes {}

impl core::fmt::Debug for CodeBytes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "[{}]", hex_bytes(self.as_slice()))
    }
}

/// フックの設置が外から得るもの。本物は `hook_win::RealHook`。止めている間に呼ばれるもの（`suspend` / `resume` / `eip` /
/// `read_code` / `write_code`）は、本物でも偽物でもヒープを使ってはいけない。
pub trait HookEnv {
    /// 自分以外のスレッドの一覧を取り、止める・再開する・EIP を読むために開く。止める前に呼ぶ（ヒープを使ってよい）。
    /// 失敗したら `GetLastError` の値。開いたハンドルは `close` で閉じる。
    fn open_other_threads(&mut self) -> Result<Vec<ThreadRef>, u32>;
    fn suspend(&mut self, t: ThreadRef) -> bool;
    fn resume(&mut self, t: ThreadRef) -> bool;
    fn close(&mut self, t: ThreadRef);
    /// 止めたスレッドの EIP。取れなければ `None`。
    fn eip(&mut self, t: ThreadRef) -> Option<u32>;
    /// 自分のプロセスのコード `[addr, addr + buf.len())` を `buf` に読む（ヒープを使わない）。
    /// コミット済みで読める頁でなければ false（`buf` の中身は当てにしない）。
    fn read_code(&mut self, addr: u32, buf: &mut [u8]) -> bool;
    /// コード `[addr, addr + bytes.len())` に書く: 頁の保護を一時的に書ける形にし、書き、保護を戻し、
    /// 命令キャッシュを流す。保護を変えられなければ 1 バイトも書かずに `Err(GetLastError)`。
    fn write_code(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32>;
    /// 入口が飛ぶ先（元の飛び先）を置く。rel32 を書く前に呼ぶ。
    fn set_original_target(&mut self, target: u32);
    /// 入口が tick ごとの記録で読むウィンドウのアドレスを置く（0 なら入口はウィンドウを読まない）。rel32 を書く前に呼ぶ。
    fn set_window(&mut self, addr: u32);
    /// 入口が枠の先頭の後ろへコピーする表を置く（0 行なら入口はコピーしない）。rel32 を書く前に呼ぶ。元が読めるかは
    /// 呼び出し側（`session`）が先に確かめる。
    fn set_copy_table(&mut self, rows: &[CopyRow]);
    /// 描画だけのフレームの出口（花映塚では `0x00431259`）を置く（0 なら入口は分かれ道を通らない）。rel32 を書く前に呼ぶ。
    /// 呼び出し側が、14 地点の照合で `render_only_exit` 行が一致したときだけそのアドレスを渡す。
    fn set_render_only_exit(&mut self, addr: u32);
    fn entry_address(&self) -> u32;
    /// 開始のフック（`0x0041AB2B`）の差し替え先のアドレス。
    fn match_entry_address(&self) -> u32;
    fn set_match_original(&mut self, target: u32);
    fn sleep_ms(&mut self, ms: u32);
    fn now_ms(&self) -> u64;
    fn counter(&self) -> u32;
}

/// 止める前に分かる「書けない」理由（表の行の形・飛び先）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    /// 表の行が call 先の差し替え（`Use::CallRel32`）でない。
    NotCallRow,
    /// 期待バイト列が `E8 rel32` の 5 バイトでない。
    Shape,
    /// 期待バイト列の rel32 から計算した飛び先が、表の元の飛び先と食い違う。
    TableTarget { computed: u32, table: u32 },
    /// 新しい rel32 を読み戻した飛び先が入口にならない（計算の誤りの守り）。
    Encode { computed: u32, entry: u32 },
    /// 差し替え先のアドレスが 0（入口が無い）。
    NoEntry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallPlan {
    pub addr: u32,
    pub expected: [u8; 5],
    pub original_target: u32,
    pub entry: u32,
    /// 書いた後の 5 バイト（`E8` ＋ 入口への rel32）。書くのは `[1..5]` の 4 バイトだけ。
    pub patched: [u8; 5],
}

impl CallPlan {
    pub fn rel32_write(&self) -> (u32, [u8; 4]) {
        let p = self.patched;
        (self.addr.wrapping_add(1), [p[1], p[2], p[3], p[4]])
    }

    /// 止めている間の手順に渡す形（EIP を見る範囲と読み直す範囲は命令の全長の 5 バイト、書くのは rel32 の 4 バイト）。
    pub fn rewrite(&self) -> Rewrite {
        let (at, rel32) = self.rel32_write();
        Rewrite {
            addr: self.addr,
            expected: CodeBytes::from(self.expected),
            write_at: at,
            write: CodeBytes::from(rel32),
        }
    }
}

/// 表の行と入口のアドレスから、書く中身を決める（`higanr_core::patch` の `call_target` / `call_rel32` を使う）。
pub fn plan_call_patch(def: &SiteDef, entry: u32) -> Result<CallPlan, PlanError> {
    let Use::CallRel32 { original_target } = def.kind else {
        return Err(PlanError::NotCallRow);
    };
    if entry == 0 {
        return Err(PlanError::NoEntry);
    }
    let expected: [u8; 5] = def.expected.try_into().map_err(|_| PlanError::Shape)?;
    let computed = patch::call_target(def.addr, &expected).ok_or(PlanError::Shape)?;
    if computed != original_target {
        return Err(PlanError::TableTarget {
            computed,
            table: original_target,
        });
    }
    let patched = patch::call_rel32(def.addr, entry);
    let back = patch::call_target(def.addr, &patched).ok_or(PlanError::Shape)?;
    if back != entry {
        return Err(PlanError::Encode {
            computed: back,
            entry,
        });
    }
    Ok(CallPlan {
        addr: def.addr,
        expected,
        original_target,
        entry,
        patched,
    })
}

/// 書き換え 1 回ぶん（止めている間の手順が使う。固定長でヒープを使わない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rewrite {
    pub addr: u32,
    /// いまあるはずの、書き換える命令の全長のバイト列。EIP を見る範囲と止めた後に読み直す範囲はこの長さ。
    pub expected: CodeBytes,
    /// 書く位置と中身（call 先の差し替えは `addr + 1` の rel32 の 4 バイト、inline detour は `addr` から全長）。
    pub write_at: u32,
    pub write: CodeBytes,
}

/// EIP が書き換える命令の全長 `[addr, addr + len)` の中か（端で回り込まない書き方）。
pub fn eip_in_range(eip: u32, addr: u32, len: usize) -> bool {
    eip >= addr && ((eip - addr) as usize) < len
}

/// 止める前に断った理由（スレッドを 1 本も止めていない。1 バイトも書いていない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Plan(PlanError),
    Unreadable,
    /// いまの 5 バイトが期待と違う。
    Mismatch(CodeBytes),
}

/// 試みの途中でやめた理由（止めたスレッドは全部再開した。1 バイトも書いていない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abort {
    /// スレッドの一覧が取れない（`GetLastError`）。
    ListFailed(u32),
    /// 何本目（0 始まり）のスレッドを止められなかったか。
    SuspendFailed { index: usize },
    /// EIP が取れないスレッドがあった（何本目か）。
    EipUnavailable { index: usize },
    /// 止めた後の読み直しが読めない。
    RereadUnreadable,
    /// 止めた後の読み直しが期待と違う（止める前の照合との間に誰かが書いた）。
    RereadMismatch(CodeBytes),
    /// 保護を変えられなかった（`GetLastError`）。
    WriteFailed(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tried {
    Aborted(Abort),
    /// `RETRY_LIMIT` 回とも、どれかのスレッドの EIP が範囲にあった（最後に見た EIP）。
    GaveUp {
        last_eip: u32,
    },
    Written(WriteDone),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Refused(Refusal),
    Aborted(Abort),
    /// `RETRY_LIMIT` 回とも、どれかのスレッドの EIP が範囲にあった（最後に見た EIP）。
    GaveUp {
        last_eip: u32,
    },
    Written(WriteDone),
}

impl From<Tried> for Outcome {
    fn from(t: Tried) -> Self {
        match t {
            Tried::Aborted(a) => Outcome::Aborted(a),
            Tried::GaveUp { last_eip } => Outcome::GaveUp { last_eip },
            Tried::Written(d) => Outcome::Written(d),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tries {
    pub attempts: u32,
    /// EIP が範囲にあって見送った回数。
    pub in_range: u32,
    pub threads: usize,
    /// 全部の試みを通して、止めたのべ数と再開したのべ数（同じでなければならない）。
    pub suspended_total: u32,
    pub resumed_total: u32,
    pub resume_failures: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallReport {
    pub outcome: Outcome,
    pub plan: Option<CallPlan>,
    pub tries: Tries,
    pub readback: Option<CodeBytes>,
}

impl InstallReport {
    pub fn written(&self) -> bool {
        matches!(self.outcome, Outcome::Written(_))
    }
}

/// 止めたスレッドの束。`Drop` で、止めたものを全部再開してからハンドルを全部閉じる（パニックで巻き戻るときも通る）。
struct Stopped<'a> {
    env: &'a mut dyn HookEnv,
    threads: Vec<ThreadRef>,
    /// `threads[..suspended]` を止めた（先頭から順に止めるので、止めたものは常に先頭の連続）。
    suspended: usize,
    suspended_total: u32,
    resumed_total: u32,
    resume_failures: u32,
}

impl<'a> Stopped<'a> {
    fn new(env: &'a mut dyn HookEnv, threads: Vec<ThreadRef>) -> Self {
        Stopped {
            env,
            threads,
            suspended: 0,
            suspended_total: 0,
            resumed_total: 0,
            resume_failures: 0,
        }
    }

    /// 先頭から順に止める。止められないスレッドがあれば、その番号を返す。
    fn suspend_all(&mut self) -> Result<(), usize> {
        for &t in &self.threads {
            if !self.env.suspend(t) {
                return Err(self.suspended);
            }
            self.suspended += 1;
            self.suspended_total += 1;
        }
        Ok(())
    }

    /// 止めた分を後ろから再開する。ヒープを使わない。
    fn resume_all(&mut self) {
        while self.suspended > 0 {
            self.suspended -= 1;
            if let Some(&t) = self.threads.get(self.suspended) {
                self.resumed_total += 1;
                if !self.env.resume(t) {
                    self.resume_failures += 1;
                }
            }
        }
    }

    fn close_all(&mut self) {
        for &t in &self.threads {
            self.env.close(t);
        }
        self.threads.clear();
    }

    fn finish(mut self) -> (u32, u32, u32) {
        self.resume_all();
        self.close_all();
        (
            self.suspended_total,
            self.resumed_total,
            self.resume_failures,
        )
    }
}

impl Drop for Stopped<'_> {
    fn drop(&mut self) {
        // 先に再開、後で閉じる（閉じたハンドルでは再開できない）。`finish` の後なら何もしない。
        self.resume_all();
        self.close_all();
    }
}

enum Attempt {
    InRange(u32),
    Stop(Abort),
    Wrote(WriteDone),
}

/// 止めている間の手順。ここから `Stopped` を手放すまで、ヒープを使わない。
fn attempt_while_stopped(s: &mut Stopped<'_>, rw: &Rewrite) -> Attempt {
    if let Err(index) = s.suspend_all() {
        return Attempt::Stop(Abort::SuspendFailed { index });
    }
    let len = rw.expected.len();
    // SuspendThread は相手が本当に止まるのを待たずに戻る。GetThreadContext は相手が止まってから文脈を
    // 返すので、全部のスレッドの EIP を読むことが「全部止まった」の確かめも兼ねる。
    for (index, &t) in s.threads.iter().enumerate() {
        match s.env.eip(t) {
            None => return Attempt::Stop(Abort::EipUnavailable { index }),
            Some(eip) if eip_in_range(eip, rw.addr, len) => return Attempt::InRange(eip),
            Some(_) => {}
        }
    }
    // 読み直しは固定長の配列に（`CodeBytes` はヒープを使わない）。`len` は `CodeBytes` の長さなので 16 以下。
    let mut now = CodeBytes {
        buf: [0u8; MAX_PATCH_LEN],
        len: rw.expected.len,
    };
    if !s.env.read_code(rw.addr, &mut now.buf[..len]) {
        return Attempt::Stop(Abort::RereadUnreadable);
    }
    if now != rw.expected {
        return Attempt::Stop(Abort::RereadMismatch(now));
    }
    match s.env.write_code(rw.write_at, rw.write.as_slice()) {
        Ok(done) => Attempt::Wrote(done),
        Err(code) => Attempt::Stop(Abort::WriteFailed(code)),
    }
}

/// 止めて書く試みを最大 `RETRY_LIMIT` 回。止める前の確かめ（表の形・いまのバイト列・入口の静的変数）は
/// 呼び出し側が済ませてから呼ぶ（call 先の差し替えと inline detour で共通）。数えは `tries` に足す。
pub fn run_attempts(env: &mut dyn HookEnv, rw: &Rewrite, tries: &mut Tries) -> Tried {
    let mut last_eip = 0;
    for attempt in 1..=RETRY_LIMIT {
        tries.attempts = attempt;
        let threads = match env.open_other_threads() {
            Ok(t) => t,
            Err(code) => return Tried::Aborted(Abort::ListFailed(code)),
        };
        tries.threads = threads.len();
        let mut stopped = Stopped::new(env, threads);
        let result = attempt_while_stopped(&mut stopped, rw);
        let (s, r, f) = stopped.finish();
        tries.suspended_total += s;
        tries.resumed_total += r;
        tries.resume_failures += f;
        match result {
            Attempt::InRange(eip) => {
                tries.in_range += 1;
                last_eip = eip;
                if attempt < RETRY_LIMIT {
                    env.sleep_ms(RETRY_WAIT_MS);
                }
            }
            // 止められない・文脈が読めないスレッドは、一覧を取った後に終わったスレッドのことがある（実機の回で、18 本のうち 17 本目を
            // 止められず crt_free の detour が書けなかった）。止めた分は再開してあるので、一覧を取り直して試し直す（最後の試みでも同じなら諦める）。
            Attempt::Stop(Abort::SuspendFailed { .. } | Abort::EipUnavailable { .. })
                if attempt < RETRY_LIMIT =>
            {
                env.sleep_ms(RETRY_WAIT_MS);
            }
            Attempt::Stop(a) => return Tried::Aborted(a),
            Attempt::Wrote(done) => return Tried::Written(done),
        }
    }
    Tried::GaveUp { last_eip }
}

/// call 先の差し替えを 1 地点に設置する。条件（同定・image base・表・全地点の照合）は呼び出し側が先に確かめる
/// （`session::ready_to_install`）。`window` は入口が tick ごとの記録で読むウィンドウのアドレス、`copies` は入口が枠へコピーする表、
/// `render_only_exit` は描画だけのフレームの出口（0 なら分かれ道を通らない）で、どれも元の飛び先と一緒に rel32 を書く前に置く。
pub fn install_call_hook(
    env: &mut dyn HookEnv,
    def: &SiteDef,
    window: u32,
    copies: &[CopyRow],
    render_only_exit: u32,
) -> InstallReport {
    let entry = env.entry_address();
    // 元の飛び先・ウィンドウのアドレス・コピーの表・出口は、この入口へ向く call を書くより前に置く
    // （置く前に入口へ来ると、0 アドレスへ飛ぶ・前のウィンドウを読む・前の表でコピーする・前の出口へ戻る）。
    install_call_hook_at(env, def, entry, &mut |e, original| {
        e.set_original_target(original);
        e.set_window(window);
        e.set_copy_table(copies);
        e.set_render_only_exit(render_only_exit);
    })
}

/// 開始のフック（`0x0041AB2B`）を設置する。手順は `install_call_hook` と同じで、差し替え先は `HookEnv::match_entry_address`、
/// 書く前に置くのは元の飛び先だけ。
pub fn install_match_hook(env: &mut dyn HookEnv, def: &SiteDef) -> InstallReport {
    let entry = env.match_entry_address();
    install_call_hook_at(env, def, entry, &mut |e, original| {
        e.set_match_original(original);
    })
}

/// call 先の差し替えを 1 地点に設置する共通の手順。`prepare` は止める前・書く前に、入口が読む静的変数（少なくとも
/// 元の飛び先）を置く。
pub fn install_call_hook_at(
    env: &mut dyn HookEnv,
    def: &SiteDef,
    entry: u32,
    prepare: &mut dyn FnMut(&mut dyn HookEnv, u32),
) -> InstallReport {
    let mut report = InstallReport {
        outcome: Outcome::Refused(Refusal::Unreadable),
        plan: None,
        tries: Tries::default(),
        readback: None,
    };
    let plan = match plan_call_patch(def, entry) {
        Ok(p) => p,
        Err(e) => {
            report.outcome = Outcome::Refused(Refusal::Plan(e));
            return report;
        }
    };
    report.plan = Some(plan);
    let mut now = [0u8; 5];
    if !env.read_code(plan.addr, &mut now) {
        report.outcome = Outcome::Refused(Refusal::Unreadable);
        return report;
    }
    if now != plan.expected {
        report.outcome = Outcome::Refused(Refusal::Mismatch(CodeBytes::from(now)));
        return report;
    }
    // 入口が読む静的変数は、この入口へ向く call を書くより前の、止める前に置く（止めている間の手順を増やさない）。
    prepare(env, plan.original_target);

    let tried = run_attempts(env, &plan.rewrite(), &mut report.tries);
    report.outcome = tried.into();
    if report.written() {
        let mut back = [0u8; 5];
        report.readback = env
            .read_code(plan.addr, &mut back)
            .then_some(CodeBytes::from(back));
    }
    report
}

fn plan_error_text(e: &PlanError) -> String {
    match e {
        PlanError::NotCallRow => "表の行が call 先の差し替えではない".to_string(),
        PlanError::Shape => "期待バイト列が E8 rel32 の 5 バイトではない".to_string(),
        PlanError::TableTarget { computed, table } => format!(
            "rel32 から計算した飛び先 0x{computed:08X} が表の元の飛び先 0x{table:08X} と違う"
        ),
        PlanError::Encode { computed, entry } => {
            format!("新しい rel32 の飛び先 0x{computed:08X} が入口 0x{entry:08X} にならない")
        }
        PlanError::NoEntry => "差し替え先の入口のアドレスが 0".to_string(),
    }
}

/// 試みの途中でやめた理由の文（`expected` は期待バイト列）。
pub fn abort_text(a: &Abort, expected: &[u8]) -> String {
    match a {
        Abort::ListFailed(code) => {
            format!("書かない（スレッドの一覧が取れない。GetLastError = {code}）")
        }
        Abort::SuspendFailed { index } => format!(
            "書かない（{} 本目のスレッドを止められない。止めた分は再開した）",
            index + 1
        ),
        Abort::EipUnavailable { index } => format!(
            "書かない（{} 本目のスレッドの EIP が取れない。全部再開した）",
            index + 1
        ),
        Abort::RereadUnreadable => {
            "書かない（止めた後の読み直しが読めない。全部再開した）".to_string()
        }
        Abort::RereadMismatch(now) => format!(
            "書かない（止めた後の読み直し {} が期待 {} と違う。全部再開した）",
            hex_bytes(now.as_slice()),
            hex_bytes(expected)
        ),
        Abort::WriteFailed(code) => {
            format!("書かない（頁の保護を変えられない。GetLastError = {code}。全部再開した）")
        }
    }
}

pub fn gave_up_text(last_eip: u32) -> String {
    format!(
        "書かない（{RETRY_LIMIT} 回とも EIP が書き換える範囲にあった。最後の EIP = 0x{last_eip:08X}）"
    )
}

/// 書いたときの文。`what` は書いた中身の説明。
pub fn written_text(done: &WriteDone, what: &str) -> String {
    format!(
        "書いた（{what}。保護の戻し = {}、命令キャッシュ = {}）",
        if done.protect_restored {
            "できた"
        } else {
            "できなかった"
        },
        if done.flushed {
            "流した"
        } else {
            "流せなかった"
        }
    )
}

pub fn tries_line(label: &str, t: &Tries) -> String {
    format!(
        "{label}の試み: {} 回（EIP が範囲にあって見送った {} 回）、最後の試みのスレッド {} 本、止めたのべ {} / 再開したのべ {} / 再開に失敗 {}",
        t.attempts, t.in_range, t.threads, t.suspended_total, t.resumed_total, t.resume_failures
    )
}

/// 設置の報告をログに書く（再開の後に呼ぶ。ヒープを使ってよい）。何も問題なく書けたときは詳細だけ。断ったとき・保護の戻しや
/// 命令キャッシュの流しに失敗したとき・再開に失敗したとき・読み返しが入口と合わないときは簡潔にも残す。
pub fn write_report(out: &mut dyn Write, def: &SiteDef, r: &InstallReport) {
    if report_is_clean(r) {
        write_report_all(&mut crate::log_mode::detail_out(out), def, r);
    } else {
        write_report_all(out, def, r);
    }
}

fn report_is_clean(r: &InstallReport) -> bool {
    let (Outcome::Written(done), Some(p)) = (r.outcome, &r.plan) else {
        return false;
    };
    let readback_ok = r
        .readback
        .and_then(|b| b.head5())
        .and_then(|b| patch::call_target(p.addr, &b))
        == Some(p.entry);
    done.protect_restored && done.flushed && r.tries.resume_failures == 0 && readback_ok
}

fn write_report_all(out: &mut dyn Write, def: &SiteDef, r: &InstallReport) {
    let _ = match &r.plan {
        Some(p) => writeln!(
            out,
            "フックの設置: 地点 {} 0x{:08X}（{}）の call の飛び先を 0x{:08X} から入口 0x{:08X} へ",
            def.name,
            def.addr,
            def.use_label(),
            p.original_target,
            p.entry
        ),
        None => writeln!(
            out,
            "フックの設置: 地点 {} 0x{:08X}（{}）",
            def.name,
            def.addr,
            def.use_label()
        ),
    };
    let head = match &r.outcome {
        Outcome::Refused(Refusal::Plan(e)) => {
            format!("書かない（止める前に断った: {}）", plan_error_text(e))
        }
        Outcome::Refused(Refusal::Unreadable) => {
            "書かない（止める前に断った: いまの 5 バイトが読めない）".to_string()
        }
        Outcome::Refused(Refusal::Mismatch(now)) => format!(
            "書かない（止める前に断った: いまの 5 バイト {} が期待 {} と違う）",
            hex_bytes(now.as_slice()),
            hex_bytes(def.expected)
        ),
        Outcome::Aborted(a) => abort_text(a, def.expected),
        Outcome::GaveUp { last_eip } => gave_up_text(*last_eip),
        Outcome::Written(done) => written_text(done, "rel32 の 4 バイト"),
    };
    let _ = writeln!(out, "フックの設置: {head}");
    let _ = writeln!(out, "{}", tries_line("フックの設置", &r.tries));
    if let (Outcome::Written(_), Some(p)) = (r.outcome, &r.plan) {
        let _ = match r.readback {
            None => writeln!(out, "読み返し: 読めない"),
            Some(back) => {
                let target = back.head5().and_then(|b| patch::call_target(p.addr, &b));
                let verdict = if target == Some(p.entry) {
                    "入口と一致"
                } else {
                    "入口と不一致"
                };
                match target {
                    Some(t) => writeln!(
                        out,
                        "読み返し: {} → 飛び先 0x{t:08X}（{verdict}）",
                        hex_bytes(back.as_slice())
                    ),
                    None => writeln!(
                        out,
                        "読み返し: {}（E8 で始まらない。{verdict}）",
                        hex_bytes(back.as_slice())
                    ),
                }
            }
        };
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! 偽の `HookEnv` で手順を見る。止めている間にヒープを使っていないかも、数える割り当て器（`test_alloc`）で見る。
    use super::*;
    use crate::test_alloc;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    /// 偽物が持つスレッドの数の上限（止めた深さを固定長の配列で数えるため）。
    const MAX_FAKE_THREADS: usize = 8;
    /// 試みの上限を外した変異で検査が永久に回らないための歯止め（`RETRY_LIMIT` より十分大きい）。
    const RUNAWAY_OPENS: u32 = 10_000;

    pub(crate) const SITE_ADDR: u32 = 0x0060_1000;
    pub(crate) const ORIGINAL: u32 = 0x0060_0800;
    pub(crate) const ENTRY: u32 = 0x1000_2000;
    pub(crate) const WINDOW: u32 = 0x0070_0B00;
    pub(crate) const COPIES: [CopyRow; 2] = [
        CopyRow {
            src: 0x0070_1000,
            len: 16,
            dst: 64,
        },
        CopyRow {
            src: 0x0070_2002,
            len: 8,
            dst: 80,
        },
    ];

    pub(crate) fn synthetic_def() -> SiteDef {
        let bytes = patch::call_rel32(SITE_ADDR, ORIGINAL);
        SiteDef {
            name: "synthetic_call",
            addr: SITE_ADDR,
            expected: Box::leak(Box::new(bytes)),
            kind: Use::CallRel32 {
                original_target: ORIGINAL,
            },
            source: "合成",
        }
    }

    type EipScript = Box<dyn FnMut(u32, usize) -> Option<u32>>;

    pub(crate) const EXIT: u32 = 0x0060_1259;

    pub(crate) struct FakeHook {
        pub mem_base: u32,
        pub mem: [u8; 16],
        /// 2 つ目のウィンドウ。call 先の差し替えと inline detour を 1 回の実行で両方書く検査用。
        pub mem2_base: u32,
        pub mem2: [u8; 16],
        /// 3 つ目のウィンドウ。開始のフックを同じ実行で書く検査用。ここへの 4 バイトの書きは、元の飛び先（`set_match`）を置いた後でなければならない。
        pub mem3_base: u32,
        pub mem3: [u8; 16],
        pub match_entry: u32,
        pub set_match: Option<u32>,
        pub threads: usize,
        pub eip: EipScript,
        pub suspend_fail: Option<(u32, usize)>,
        /// 最初に止めたとき、ウィンドウの中身をこれに替える（止める前の照合と止めた後の間に誰かが書いた形）。
        pub mutate_on_first_suspend: Option<[u8; 5]>,
        pub panic_in_eip: bool,
        pub write_fail: Option<u32>,
        pub entry: u32,
        pub now: u64,
        pub counter_fn: fn(u64) -> u32,

        pub opens: u32,
        pub suspends: u32,
        pub resumes: u32,
        pub closes: u32,
        pub opened_handles: u32,
        pub eip_calls: u32,
        pub writes: u32,
        pub written_at: Option<u32>,
        pub set_original: Option<u32>,
        pub set_window: Option<u32>,
        pub set_copies: Option<Vec<CopyRow>>,
        pub set_exit: Option<u32>,
        /// 入口の静的変数を置く前に書いた回数（手順の順の誤り）。
        pub writes_before_setup: u32,
        pub tramp_addr: u32,
        pub tramp_alloc_fail: Option<u32>,
        pub tramp_commit_fail: Option<u32>,
        pub tramp_allocs: u32,
        pub tramp_committed: Option<(u32, Vec<u8>)>,
        pub tramp_released: Vec<u32>,
        pub tramp_set: Vec<u32>,
        /// 止めている間にトランポリンを確保・書き・返した回数（手順の順の誤り）。
        pub tramp_while_stopped: u32,
        pub detour_entry: u32,
        pub sleeps: Vec<u32>,
        pub depth: [u32; MAX_FAKE_THREADS],
        pub stopped_now: u32,
        alloc_at_stop: u64,
        /// 止めている間に割り当て・解放があった回数（全部再開したときに数える）。
        pub alloc_violations: u32,
        /// 止めていない間に EIP・読み直し・書き込みが呼ばれた回数（手順の順の誤り）。
        pub unstopped_ops: u32,
        pub called_any: u32,
    }

    impl FakeHook {
        pub(crate) fn new(threads: usize) -> Self {
            assert!(threads <= MAX_FAKE_THREADS);
            let def = synthetic_def();
            let mut mem = [0x90u8; 16];
            mem[..5].copy_from_slice(def.expected);
            FakeHook {
                mem_base: SITE_ADDR,
                mem,
                mem2_base: 0,
                mem2: [0x90; 16],
                mem3_base: 0,
                mem3: [0x90; 16],
                match_entry: 0x1000_4000,
                set_match: None,
                threads,
                eip: Box::new(|_, i| Some(0x0070_0000 + i as u32 * 0x100)),
                suspend_fail: None,
                mutate_on_first_suspend: None,
                panic_in_eip: false,
                write_fail: None,
                entry: ENTRY,
                now: 1_000_000,
                counter_fn: |_| 0,
                opens: 0,
                suspends: 0,
                resumes: 0,
                closes: 0,
                opened_handles: 0,
                eip_calls: 0,
                writes: 0,
                written_at: None,
                set_original: None,
                set_window: None,
                set_copies: None,
                set_exit: None,
                writes_before_setup: 0,
                tramp_addr: 0x0080_0000,
                tramp_alloc_fail: None,
                tramp_commit_fail: None,
                tramp_allocs: 0,
                tramp_committed: None,
                tramp_released: Vec::new(),
                tramp_set: Vec::new(),
                tramp_while_stopped: 0,
                detour_entry: 0x1000_3000,
                sleeps: Vec::new(),
                depth: [0; MAX_FAKE_THREADS],
                stopped_now: 0,
                alloc_at_stop: 0,
                alloc_violations: 0,
                unstopped_ops: 0,
                called_any: 0,
            }
        }

        fn region(&mut self, addr: u32, len: usize) -> Option<&mut [u8]> {
            let inside = |base: u32| {
                let off = addr.checked_sub(base)? as usize;
                (off + len <= 16).then_some(off)
            };
            if let Some(off) = inside(self.mem_base) {
                return Some(&mut self.mem[off..off + len]);
            }
            if self.mem2_base != 0
                && let Some(off) = inside(self.mem2_base)
            {
                return Some(&mut self.mem2[off..off + len]);
            }
            if self.mem3_base != 0
                && let Some(off) = inside(self.mem3_base)
            {
                return Some(&mut self.mem3[off..off + len]);
            }
            None
        }

        /// 書く前に置くべき入口の静的変数が置かれているか（`bytes_len` が 4 なら call 先の差し替え、5 以上なら inline detour。3 つ目のウィンドウは開始のフック）。
        fn setup_done(&self, addr: u32, bytes_len: usize) -> bool {
            if self.mem3_base != 0 && addr.wrapping_sub(self.mem3_base) < 16 {
                self.set_match.is_some()
            } else if bytes_len == 4 {
                self.set_window.is_some()
                    && self.set_original.is_some()
                    && self.set_copies.is_some()
                    && self.set_exit.is_some()
            } else {
                let committed = self.tramp_committed.as_ref().map(|(a, _)| *a);
                self.tramp_set
                    .last()
                    .is_some_and(|&t| t != 0 && Some(t) == committed)
            }
        }

        fn thread_index(t: ThreadRef) -> usize {
            t.0 - 1
        }

        pub(crate) fn all_resumed(&self) -> bool {
            self.depth.iter().all(|&d| d == 0) && self.stopped_now == 0
        }
    }

    impl HookEnv for FakeHook {
        fn open_other_threads(&mut self) -> Result<Vec<ThreadRef>, u32> {
            self.called_any += 1;
            self.opens += 1;
            assert!(
                self.opens <= RUNAWAY_OPENS,
                "試みが止まらない（上限が効いていない）"
            );
            self.opened_handles += self.threads as u32;
            Ok((1..=self.threads).map(ThreadRef).collect())
        }
        fn suspend(&mut self, t: ThreadRef) -> bool {
            self.called_any += 1;
            let i = Self::thread_index(t);
            if self.suspend_fail == Some((self.opens, i)) || self.suspend_fail == Some((0, i)) {
                return false;
            }
            if self.stopped_now == 0 {
                self.alloc_at_stop = test_alloc::count();
                if let Some(bytes) = self.mutate_on_first_suspend.take() {
                    self.mem[..5].copy_from_slice(&bytes);
                }
            }
            self.stopped_now += 1;
            self.suspends += 1;
            if let Some(d) = self.depth.get_mut(i) {
                *d += 1;
            }
            true
        }
        fn resume(&mut self, t: ThreadRef) -> bool {
            self.called_any += 1;
            let i = Self::thread_index(t);
            if let Some(d) = self.depth.get_mut(i) {
                *d = d.saturating_sub(1);
            }
            self.resumes += 1;
            self.stopped_now = self.stopped_now.saturating_sub(1);
            if self.stopped_now == 0 && test_alloc::count() != self.alloc_at_stop {
                self.alloc_violations += 1;
            }
            true
        }
        fn close(&mut self, _t: ThreadRef) {
            self.called_any += 1;
            self.closes += 1;
        }
        fn eip(&mut self, t: ThreadRef) -> Option<u32> {
            self.called_any += 1;
            self.eip_calls += 1;
            if self.stopped_now == 0 || self.stopped_now as usize != self.threads {
                self.unstopped_ops += 1;
            }
            if self.panic_in_eip {
                panic!("偽物: 止めている間のパニック");
            }
            (self.eip)(self.opens, Self::thread_index(t))
        }
        fn read_code(&mut self, addr: u32, buf: &mut [u8]) -> bool {
            self.called_any += 1;
            match self.region(addr, buf.len()) {
                Some(r) => {
                    buf.copy_from_slice(r);
                    true
                }
                None => false,
            }
        }
        fn write_code(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32> {
            self.called_any += 1;
            if self.stopped_now as usize != self.threads {
                self.unstopped_ops += 1;
            }
            if !self.setup_done(addr, bytes.len()) {
                self.writes_before_setup += 1;
            }
            if let Some(code) = self.write_fail {
                return Err(code);
            }
            self.region(addr, bytes.len())
                .expect("偽物: 窓の外に書いた")
                .copy_from_slice(bytes);
            self.writes += 1;
            self.written_at = Some(addr);
            Ok(WriteDone {
                protect_restored: true,
                flushed: true,
            })
        }
        fn set_original_target(&mut self, target: u32) {
            self.called_any += 1;
            assert_eq!(self.writes, 0, "元の飛び先を書いた後に置いた");
            self.set_original = Some(target);
        }
        fn set_window(&mut self, addr: u32) {
            self.called_any += 1;
            self.set_window = Some(addr);
        }
        fn set_copy_table(&mut self, rows: &[CopyRow]) {
            self.called_any += 1;
            self.set_copies = Some(rows.to_vec());
        }
        fn set_render_only_exit(&mut self, addr: u32) {
            self.called_any += 1;
            self.set_exit = Some(addr);
        }
        fn entry_address(&self) -> u32 {
            self.entry
        }
        fn match_entry_address(&self) -> u32 {
            self.match_entry
        }
        fn set_match_original(&mut self, target: u32) {
            self.called_any += 1;
            self.set_match = Some(target);
        }
        fn sleep_ms(&mut self, ms: u32) {
            self.sleeps.push(ms);
            self.now += ms as u64;
        }
        fn now_ms(&self) -> u64 {
            self.now
        }
        fn counter(&self) -> u32 {
            (self.counter_fn)(self.now)
        }
    }

    fn log_of(def: &SiteDef, r: &InstallReport) -> String {
        let mut out = Vec::new();
        write_report(&mut out, def, r);
        String::from_utf8(out).expect("ログは UTF-8")
    }

    /// どの経路でも守ること: 止めた数と再開した数が同じ、開いたハンドルを全部閉じた、止めている間に
    /// ヒープを使っていない、止めていない間に EIP を読んだり書いたりしていない。
    fn assert_balanced(f: &FakeHook, r: &InstallReport) {
        assert_eq!(f.suspends, f.resumes, "止めた数と再開した数");
        assert!(f.all_resumed(), "止めたままのスレッドがある: {:?}", f.depth);
        assert_eq!(f.opened_handles, f.closes, "開いたハンドルと閉じた数");
        assert_eq!(r.tries.suspended_total, f.suspends);
        assert_eq!(r.tries.resumed_total, f.resumes);
        assert_eq!(f.alloc_violations, 0, "止めている間にヒープを使った");
        assert_eq!(f.unstopped_ops, 0, "全部を止めずに EIP を読んだ・書いた");
        assert_eq!(
            f.writes_before_setup, 0,
            "窓のアドレス・元の飛び先・コピーの表・出口を置く前に書いた"
        );
    }

    #[test]
    fn plan_encodes_rel32_forward_and_backward() {
        let def = synthetic_def();
        let p = plan_call_patch(&def, 0x1000_2000).unwrap();
        assert_eq!(p.patched, [0xE8, 0xFB, 0x0F, 0xA0, 0x0F]);
        assert_eq!(p.rel32_write(), (SITE_ADDR + 1, [0xFB, 0x0F, 0xA0, 0x0F]));
        let p = plan_call_patch(&def, 0x0001_0000).unwrap();
        assert_eq!(p.patched, [0xE8, 0xFB, 0xEF, 0xA0, 0xFF]);
        assert_eq!(p.rel32_write(), (SITE_ADDR + 1, [0xFB, 0xEF, 0xA0, 0xFF]));
        assert_eq!(p.original_target, ORIGINAL);
        assert_eq!(p.expected, [0xE8, 0xFB, 0xF7, 0xFF, 0xFF]);
    }

    #[test]
    fn plan_rejects_bad_shapes_and_target_mismatch() {
        let def = synthetic_def();
        let e9 = SiteDef {
            expected: &[0xE9, 0xFB, 0xF7, 0xFF, 0xFF],
            ..def
        };
        assert_eq!(plan_call_patch(&e9, ENTRY), Err(PlanError::Shape));
        let short = SiteDef {
            expected: &[0xE8, 0xFB, 0xF7, 0xFF],
            ..def
        };
        assert_eq!(plan_call_patch(&short, ENTRY), Err(PlanError::Shape));
        let long = SiteDef {
            expected: &[0xE8, 0xFB, 0xF7, 0xFF, 0xFF, 0x90],
            ..def
        };
        assert_eq!(plan_call_patch(&long, ENTRY), Err(PlanError::Shape));
        let detour = SiteDef {
            kind: Use::VerifyOnly { instr_lens: None },
            ..def
        };
        assert_eq!(plan_call_patch(&detour, ENTRY), Err(PlanError::NotCallRow));
        let wrong = SiteDef {
            kind: Use::CallRel32 {
                original_target: ORIGINAL + 1,
            },
            ..def
        };
        assert_eq!(
            plan_call_patch(&wrong, ENTRY),
            Err(PlanError::TableTarget {
                computed: ORIGINAL,
                table: ORIGINAL + 1
            })
        );
    }

    #[test]
    fn eip_range_is_the_whole_length_of_the_rewrite() {
        assert!(eip_in_range(SITE_ADDR, SITE_ADDR, 5));
        assert!(eip_in_range(SITE_ADDR + 4, SITE_ADDR, 5));
        assert!(!eip_in_range(SITE_ADDR + 5, SITE_ADDR, 5));
        assert!(!eip_in_range(SITE_ADDR - 1, SITE_ADDR, 5));
        assert!(!eip_in_range(0, SITE_ADDR, 5));
        assert!(!eip_in_range(u32::MAX, 0xFFFF_FFF0, 5));
        assert!(eip_in_range(u32::MAX, 0xFFFF_FFFC, 5));
        assert!(eip_in_range(SITE_ADDR + 5, SITE_ADDR, 9));
        assert!(eip_in_range(SITE_ADDR + 8, SITE_ADDR, 9));
        assert!(!eip_in_range(SITE_ADDR + 9, SITE_ADDR, 9));
    }

    #[test]
    fn writes_rel32_when_no_eip_is_in_range() {
        let def = synthetic_def();
        let mut f = FakeHook::new(3);
        let r = install_call_hook(&mut f, &def, WINDOW, &COPIES, EXIT);
        assert_eq!(
            r.outcome,
            Outcome::Written(WriteDone {
                protect_restored: true,
                flushed: true
            })
        );
        assert_eq!(f.writes, 1);
        assert_eq!(f.written_at, Some(SITE_ADDR + 1));
        assert_eq!(f.set_original, Some(ORIGINAL));
        assert_eq!(f.set_window, Some(WINDOW));
        assert_eq!(f.set_copies, Some(COPIES.to_vec()));
        assert_eq!(r.tries.attempts, 1);
        assert_eq!(r.tries.threads, 3);
        let want = patch::call_rel32(SITE_ADDR, ENTRY);
        assert_eq!(f.mem[..5], want);
        assert_eq!(r.readback, Some(CodeBytes::from(want)));
        assert_eq!(f.suspends, 3);
        assert_balanced(&f, &r);
        let log = log_of(&def, &r);
        assert!(
            log.contains("フックの設置: 書いた（rel32 の 4 バイト。保護の戻し = できた、命令キャッシュ = 流した）"),
            "{log}"
        );
        assert!(
            log.contains(&format!(
                "読み返し: {} → 飛び先 0x{ENTRY:08X}（入口と一致）",
                hex_bytes(&want)
            )),
            "{log}"
        );
        assert!(log.contains(
            "フックの設置の試み: 1 回（EIP が範囲にあって見送った 0 回）、最後の試みのスレッド 3 本、止めたのべ 3 / 再開したのべ 3 / 再開に失敗 0"
        ));
    }

    #[test]
    fn match_hook_points_at_its_own_entry() {
        const MSITE: u32 = 0x0060_2000;
        const MORIG: u32 = 0x0060_2800;
        let bytes = patch::call_rel32(MSITE, MORIG);
        let def = SiteDef {
            name: "synthetic_match",
            addr: MSITE,
            expected: Box::leak(Box::new(bytes)),
            kind: Use::CallRel32 {
                original_target: MORIG,
            },
            source: "合成",
        };
        let mut f = FakeHook::new(2);
        f.mem3_base = MSITE;
        f.mem3[..5].copy_from_slice(&bytes);
        let r = install_match_hook(&mut f, &def);
        assert!(r.written(), "{:?}", r.outcome);
        assert_eq!(f.written_at, Some(MSITE + 1));
        assert_eq!(f.mem3[..5], patch::call_rel32(MSITE, f.match_entry));
        assert_eq!(f.set_match, Some(MORIG));
        assert_eq!(f.set_original, None, "入力の入口の元の飛び先に触った");
        assert_balanced(&f, &r);
        let mut f = FakeHook::new(2);
        f.mem3_base = MSITE;
        f.mem3[..5].copy_from_slice(&bytes);
        f.match_entry = 0;
        let r = install_match_hook(&mut f, &def);
        assert_eq!(
            r.outcome,
            Outcome::Refused(Refusal::Plan(PlanError::NoEntry))
        );
        assert_eq!((f.writes, f.suspends), (0, 0));
        assert!(log_of(&def, &r).contains("差し替え先の入口のアドレスが 0"));
    }

    #[test]
    fn retries_while_an_eip_is_in_range_then_writes() {
        let def = synthetic_def();
        let mut f = FakeHook::new(3);
        f.eip = Box::new(|attempt, i| {
            if i == 1 && attempt <= 3 {
                Some(SITE_ADDR + 2)
            } else {
                Some(0x0070_0000)
            }
        });
        let r = install_call_hook(&mut f, &def, WINDOW, &COPIES, EXIT);
        assert!(r.written());
        assert_eq!(r.tries.attempts, 4);
        assert_eq!(r.tries.in_range, 3);
        assert_eq!(f.writes, 1);
        assert_eq!(f.sleeps, vec![RETRY_WAIT_MS; 3]);
        assert_eq!(f.opens, 4);
        assert_eq!(f.suspends, 12);
        assert_balanced(&f, &r);
    }

    #[test]
    fn gives_up_after_the_retry_limit_without_writing() {
        let def = synthetic_def();
        let mut f = FakeHook::new(2);
        f.eip = Box::new(|_, i| Some(if i == 0 { SITE_ADDR } else { 0x0070_0000 }));
        let r = install_call_hook(&mut f, &def, WINDOW, &COPIES, EXIT);
        assert_eq!(
            r.outcome,
            Outcome::GaveUp {
                last_eip: SITE_ADDR
            }
        );
        assert_eq!(f.writes, 0);
        assert_eq!(f.opens, RETRY_LIMIT);
        assert_eq!(r.tries.attempts, RETRY_LIMIT);
        assert_eq!(r.tries.in_range, RETRY_LIMIT);
        assert_eq!(f.sleeps.len() as u32, RETRY_LIMIT - 1);
        assert_eq!(f.mem[..5], *def.expected);
        assert_balanced(&f, &r);
        assert!(log_of(&def, &r).contains(&format!(
            "書かない（50 回とも EIP が書き換える範囲にあった。最後の EIP = 0x{SITE_ADDR:08X}）"
        )));
    }

    #[test]
    fn eip_unavailable_aborts_without_writing() {
        let def = synthetic_def();
        let mut f = FakeHook::new(3);
        f.eip = Box::new(|_, i| if i == 2 { None } else { Some(0x0070_0000) });
        let r = install_call_hook(&mut f, &def, WINDOW, &COPIES, EXIT);
        assert_eq!(
            r.outcome,
            Outcome::Aborted(Abort::EipUnavailable { index: 2 })
        );
        assert_eq!(f.writes, 0);
        assert_eq!(f.opens, RETRY_LIMIT);
        assert_eq!(f.mem[..5], *def.expected);
        assert_balanced(&f, &r);
        assert!(
            log_of(&def, &r)
                .contains("書かない（3 本目のスレッドの EIP が取れない。全部再開した）")
        );
    }

    #[test]
    fn reread_mismatch_after_stopping_aborts_without_writing() {
        let def = synthetic_def();
        let mut f = FakeHook::new(2);
        let other = patch::call_rel32(SITE_ADDR, 0x0055_0000);
        f.mutate_on_first_suspend = Some(other);
        let r = install_call_hook(&mut f, &def, WINDOW, &COPIES, EXIT);
        assert_eq!(
            r.outcome,
            Outcome::Aborted(Abort::RereadMismatch(CodeBytes::from(other)))
        );
        assert_eq!(f.writes, 0);
        assert_eq!(f.mem[..5], other);
        assert_balanced(&f, &r);
        assert!(log_of(&def, &r).contains(&format!(
            "書かない（止めた後の読み直し {} が期待 {} と違う。全部再開した）",
            hex_bytes(&other),
            hex_bytes(def.expected)
        )));
    }

    #[test]
    fn mismatch_before_stopping_touches_nothing() {
        let def = synthetic_def();
        let mut f = FakeHook::new(2);
        f.mem[3] ^= 0x01;
        let r = install_call_hook(&mut f, &def, WINDOW, &COPIES, EXIT);
        assert!(matches!(r.outcome, Outcome::Refused(Refusal::Mismatch(_))));
        assert_eq!((f.opens, f.suspends, f.writes), (0, 0, 0));
        assert_eq!(f.set_original, None);
        assert_eq!(f.set_window, None, "書かないのに窓のアドレスを置いた");
        assert_eq!(f.set_copies, None, "書かないのにコピーの表を置いた");
        assert_eq!(f.set_exit, None, "書かないのに出口を置いた");
        assert_eq!(r.tries.attempts, 0);
    }

    #[test]
    fn window_and_copies_are_set_before_the_rel32_is_written() {
        let def = synthetic_def();
        for (w, c, x) in [(WINDOW, &COPIES[..], EXIT), (0, &[][..], 0)] {
            let mut f = FakeHook::new(2);
            let r = install_call_hook(&mut f, &def, w, c, x);
            assert!(r.written());
            assert_eq!(f.set_window, Some(w));
            assert_eq!(f.set_copies.as_deref(), Some(c));
            assert_eq!(f.set_exit, Some(x));
            assert_eq!(f.writes, 1);
            assert_eq!(
                f.writes_before_setup, 0,
                "窓のアドレス・コピーの表・出口を置く前に書いた"
            );
        }
    }

    #[test]
    fn suspend_failure_resumes_and_retries_with_a_fresh_list() {
        let def = synthetic_def();
        let mut f = FakeHook::new(4);
        f.suspend_fail = Some((1, 2));
        let r = install_call_hook(&mut f, &def, WINDOW, &COPIES, EXIT);
        assert!(matches!(r.outcome, Outcome::Written(_)), "{:?}", r.outcome);
        assert_eq!(f.writes, 1);
        assert_eq!(f.opens, 2);
        assert_eq!(r.tries.attempts, 2);
        // 1 回目: 0 と 1 を止めて 2 で止まる。2 回目: 4 本とも止める。
        assert_eq!(f.suspends, 2 + 4);
        assert_balanced(&f, &r);
    }

    #[test]
    fn suspend_failure_every_time_gives_up_without_writing() {
        let def = synthetic_def();
        let mut f = FakeHook::new(4);
        f.suspend_fail = Some((0, 2));
        let r = install_call_hook(&mut f, &def, WINDOW, &COPIES, EXIT);
        assert_eq!(
            r.outcome,
            Outcome::Aborted(Abort::SuspendFailed { index: 2 })
        );
        assert_eq!(f.writes, 0);
        assert_eq!(f.opens, RETRY_LIMIT);
        assert_eq!(f.mem[..5], *def.expected);
        assert_balanced(&f, &r);
    }

    #[test]
    fn write_failure_is_reported_and_threads_resume() {
        let def = synthetic_def();
        let mut f = FakeHook::new(2);
        f.write_fail = Some(5);
        let r = install_call_hook(&mut f, &def, WINDOW, &COPIES, EXIT);
        assert_eq!(r.outcome, Outcome::Aborted(Abort::WriteFailed(5)));
        assert_eq!(r.readback, None);
        assert_balanced(&f, &r);
    }

    #[test]
    fn panic_while_stopped_still_resumes_everything() {
        let def = synthetic_def();
        let mut f = FakeHook::new(3);
        f.panic_in_eip = true;
        let r = catch_unwind(AssertUnwindSafe(|| {
            install_call_hook(&mut f, &def, WINDOW, &COPIES, EXIT)
        }));
        assert!(r.is_err());
        assert_eq!(f.suspends, 3);
        assert_eq!(f.resumes, 3);
        assert!(f.all_resumed());
        assert_eq!(f.opened_handles, f.closes);
        assert_eq!(f.writes, 0);
    }
}
