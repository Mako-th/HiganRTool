//! 検査だけの偽物: ゲームのウィンドウの模型（`FakeGame`）と、仮の時計で動くネットワーク（`FakeNet`）。
//!
//! `FakeGame` が持つゲームの動き:
//! - 入力の地点の後、同じ tick の優先度 0 の後半で場面の切替が走る（今 1・次 2 → 場面 2 と読み込み、今 2・次 10 → 場面 2 を作り直して
//!   読み込み、今 2・次 1 → メニュー）。**描画だけのフレームでは、切替も更新も走らない。**
//! - 読み込みは `load_*` tick。読み込みを終えた後の最初の更新の中で開始のフック（`on_match_start`）が実行、次の tick が frame 0。
//! - マッチの中の更新は feed された 3 本から「乱数」を進める（CHECK をゲームから読むときの値）。seg の決めたフレーム数（`seg_frames`）を使うと、
//!   最後のフレームの更新が次の要求を書く（再戦 10、最後なら 1）。
//! - メニューでは、繋がって選択に入ってから `menu_before_decide` tick で「マップの決定」（次の要求 2）を書く。
//!
//! 偽物の中で起きた食い違い（feed した 3 本が返しと合わない、など）は、その場で落とす。
#![forbid(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::rc::Rc;

use higanr_core::link::Outlet;
use higanr_core::lockstep::Role;
use higanr_core::selection::Agreed;
use higanr_core::wire::{MAX_DATAGRAM, Seat, Select};

use super::menu_tests::Driver;
use super::{
    Action, BadValue, FrameRec, FrameTiming, Game, Net, Phase, REQ_LEAVE_MATCH, REQ_REBUILD_MATCH,
};
use crate::scripted::SCENE_MENU;

use super::SCENE_MATCH;

pub const FRAME_US: u64 = 16_667;

fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `seed` の偽のゲームが n 回目（1 から）に読むローカル入力。`SEND_MASK`（0x00FF）の中で 0 を避ける。
pub fn local_word(seed: u64, n: u64) -> u16 {
    (mix(seed ^ (n << 20)) % 255) as u16 + 1
}

pub struct FakeGame {
    pub cur: u32,
    pub next: u32,
    menu_ticks: u32,
    pub menu_before_decide: u32,
    pub load_first: u32,
    pub load_rematch: u32,
    loading_left: u32,
    in_match: bool,
    pub frames_per_seg: u32,
    /// 空でなければ、n 番目（0 から）のマッチのフレーム数（マッチごとに長さが違う形。尽きたら `frames_per_seg`）。
    pub seg_frames: Vec<u32>,
    segs_done: usize,
    /// 1 回の選択で続けるマッチの数（2 つ目からは再戦）。
    pub segs_per_session: u32,
    /// 選択からマッチまで（と、再戦の続き）を何回するか。回の間はマッチを出てメニューへ戻る。
    pub sessions: u32,
    /// 回の終わりにマッチを出る次の要求（既定 1 = キャラを変更。6 = リプレイを保存して終了）。
    pub leave_req: u32,
    frames_in_seg: u32,
    segs_in_session: u32,
    sessions_done: u32,
    pub done: bool,
    seed: u64,
    word_n: u64,
    pub local_reads: u64,
    fed: [Option<u16>; 3],
    pub recs: Vec<FrameRec>,
    pub timings: Vec<FrameTiming>,
    pub render_only: u64,
    pub zero: u64,
    pub local: u64,
    pub rng: u64,
    /// マッチのフレームを使った数がこれになったら、乱数を 1 ビット壊す（ズレの検査）。
    pub corrupt_at: Option<u64>,
    pub select_chara: u8,
    /// 自分の SELECT に載せるマップ（両者で違えばホストの値を採る）。
    pub select_map: u8,
    pub agreed: Vec<(Seat, Agreed)>,
    pub charges: Vec<(u8, u8)>,
    pub match_starts: u32,
    /// `Some` なら `apply_agreed` がこの値で断る（範囲の外の SELECT の検査）。
    pub reject_agreed: Option<BadValue>,
    /// `local_input` に渡された席（bit 0 = 1P、bit 1 = 2P）。
    pub seats_seen: u8,
    /// 人が Esc を押している（`peek_local_input` が `ESC` を返す）。同期ズレで止めたマッチの Esc の検査用。
    pub esc_down: bool,
    /// `local_input` の語に足すビット（既定 0）。マスクの内外のビットを人が押した形の検査用。
    pub local_or: u16,
    /// 数字キー 0〜9・Tab・Ctrl・Alt/Win の今の押下の集合（`Game::digit_keys`。bit の並びは `digit_keys`）。`None` は読んではいけない間。検査が tick の合間に置く。
    pub digit_held: Option<u16>,
    /// `digit_keys` を呼ばれた回数（1 tick に 1 回、読んでよい phase だけ —— マッチの外は `dispatch`、マッチの間は取得で）。
    pub digit_reads: u64,
    /// 台本の付いている実行（`Game::scripted_run`）の印。検査が立てる（既定は false = 台本の無い実行）。
    pub scripted: bool,
    /// 台本の段階の押下（`Game::digit_script`）。検査が置き、取り出したら空になる（1 回だけ）。既定は `None`。
    pub script_press: Option<u16>,
}

impl FakeGame {
    pub fn new(seed: u64, frames_per_seg: u32, segs_per_session: u32, sessions: u32) -> Self {
        FakeGame {
            cur: SCENE_MENU,
            next: 0,
            menu_ticks: 0,
            menu_before_decide: 20,
            load_first: 58,
            load_rematch: 1,
            loading_left: 0,
            in_match: false,
            frames_per_seg,
            seg_frames: Vec::new(),
            segs_done: 0,
            segs_per_session,
            sessions,
            leave_req: 1,
            frames_in_seg: 0,
            segs_in_session: 0,
            sessions_done: 0,
            done: false,
            seed,
            word_n: 0,
            local_reads: 0,
            fed: [None; 3],
            recs: Vec::new(),
            timings: Vec::new(),
            render_only: 0,
            zero: 0,
            local: 0,
            rng: 0x1234_5678,
            corrupt_at: None,
            select_chara: (seed % 16) as u8,
            select_map: 1,
            // 割り当ての検査で、`on_input` の中から呼ばれる push が伸びないように先に広げておく。
            agreed: Vec::with_capacity(64),
            charges: Vec::with_capacity(64),
            match_starts: 0,
            reject_agreed: None,
            seats_seen: 0,
            esc_down: false,
            local_or: 0,
            digit_held: None,
            digit_reads: 0,
            scripted: false,
            script_press: None,
        }
    }

    pub fn do_frame<P: Driver, N: Net>(&mut self, np: &mut P, net: &mut N) -> Action {
        self.begin();
        let a = np.on_input(self, net);
        self.after(a, np);
        a
    }

    pub fn begin(&mut self) {
        self.fed = [None; 3];
    }

    pub fn after<P: Driver>(&mut self, a: Action, np: &mut P) {
        let mut played = false;
        match a {
            Action::RenderOnly => {
                if np.phase() == Phase::Lockstep {
                    assert_eq!(self.fed, [None; 3], "描画だけのフレームで feed した");
                }
                self.render_only += 1;
                return;
            }
            Action::Local => {
                assert_eq!(self.fed, [None; 3], "lockstep の外で feed した");
                self.local += 1;
            }
            Action::Zero => {
                assert_eq!(self.fed, [Some(0); 3], "0 の tick の feed が違う");
                self.zero += 1;
            }
            Action::Menu => panic!("選択の画面を持たない偽のゲームでネットの選択が動いた"),
            Action::Played(r) => {
                assert_eq!(
                    self.fed,
                    [Some(r.p1), Some(r.p2), Some(r.p1 | r.p2)],
                    "feed した 3 本が返しと違う"
                );
                assert!(self.in_match, "開始の前に試合のフレームを使った");
                self.recs.push(r);
                self.timings.push(np.last_timing());
                self.rng = mix(self.rng ^ (u64::from(r.p1) << 16) ^ u64::from(r.p2));
                if self.corrupt_at == Some(self.recs.len() as u64) {
                    self.rng ^= 1;
                }
                played = true;
            }
        }
        self.update(np, played);
    }

    fn update<P: Driver>(&mut self, np: &mut P, played: bool) {
        if self.cur == SCENE_MENU && self.next == SCENE_MATCH {
            self.cur = SCENE_MATCH;
            self.next = 0;
            self.loading_left = self.load_first;
            self.in_match = false;
            self.frames_in_seg = 0;
            return;
        }
        if self.cur == SCENE_MATCH && REQ_REBUILD_MATCH.contains(&self.next) {
            self.next = 0;
            self.loading_left = self.load_rematch;
            self.in_match = false;
            self.frames_in_seg = 0;
            return;
        }
        if self.cur == SCENE_MATCH && REQ_LEAVE_MATCH.contains(&self.next) {
            self.cur = SCENE_MENU;
            self.next = 0;
            self.in_match = false;
            self.menu_ticks = 0;
            return;
        }
        if self.cur == SCENE_MENU {
            if !self.done && np.phase() == Phase::Selecting {
                self.menu_ticks += 1;
                if self.menu_ticks >= self.menu_before_decide {
                    self.next = SCENE_MATCH;
                }
            }
            return;
        }
        if self.loading_left > 0 {
            self.loading_left -= 1;
            if self.loading_left == 0 {
                np.on_match_start(self);
                self.in_match = true;
                self.match_starts += 1;
            }
            return;
        }
        if self.in_match && played {
            self.frames_in_seg += 1;
            let len = self
                .seg_frames
                .get(self.segs_done)
                .copied()
                .unwrap_or(self.frames_per_seg);
            if self.frames_in_seg >= len {
                self.segs_done += 1;
                self.segs_in_session += 1;
                if self.segs_in_session < self.segs_per_session {
                    self.next = 10;
                } else {
                    self.segs_in_session = 0;
                    self.sessions_done += 1;
                    if self.sessions_done >= self.sessions {
                        self.done = true;
                    }
                    self.next = self.leave_req;
                }
            }
        }
    }
}

impl Game for FakeGame {
    fn scenes(&mut self) -> (u32, u32) {
        (self.cur, self.next)
    }

    fn local_input(&mut self, seat: Seat) -> u16 {
        self.seats_seen |= 1 << (seat as u8);
        self.word_n += 1;
        self.local_reads += 1;
        local_word(self.seed, self.word_n) | self.local_or
    }

    fn digit_keys(&mut self) -> Option<u16> {
        self.digit_reads += 1;
        self.digit_held
    }

    fn digit_script(&mut self) -> Option<u16> {
        self.script_press.take()
    }

    fn scripted_run(&self) -> bool {
        self.scripted
    }

    /// 観戦者の Esc の読み。この偽物の `local_input` は Esc を含む疑似乱数の語を返すので、そのまま読むと観戦者が勝手に抜ける。
    /// 人は既定では何も押していない（0。`esc_down` を立てた検査だけ Esc）。`word_n` も進めない。
    fn peek_local_input(&mut self) -> u16 {
        if self.esc_down {
            higanr_core::input_model::keys::ESC
        } else {
            0
        }
    }

    fn feed(&mut self, index: u32, word: u16) {
        if let Some(f) = self.fed.get_mut(index as usize) {
            assert!(f.is_none(), "同じ tick に index {index} を 2 回 feed した");
            *f = Some(word);
        }
    }

    fn own_select(&mut self, _seat: Seat, role: Role) -> Select {
        Select {
            match_no: 0,
            chara: self.select_chara,
            color: 0,
            life: 10,
            charge: self.select_chara % 3,
            map: self.select_map,
            difficulty: 2,
            cpu_1p: 0,
            cpu_2p: 0,
            background: 0,
            rng_seed: if role == Role::Host { 777 } else { 0 },
            option_digest: 0,
        }
    }

    fn apply_agreed(&mut self, peer_seat: Seat, agreed: &Agreed) -> Result<(), BadValue> {
        if let Some(bad) = self.reject_agreed {
            return Err(bad);
        }
        self.agreed.push((peer_seat, *agreed));
        Ok(())
    }

    fn write_charge(&mut self, p1: u8, p2: u8) -> bool {
        self.charges.push((p1, p2));
        true
    }

    fn check_values(&mut self) -> (u16, u32, u8, u8) {
        (self.rng as u16, (self.rng >> 16) as u32, 10, 10)
    }
}

#[derive(Clone, Copy)]
struct Packet {
    due: u64,
    from: SocketAddr,
    len: usize,
    bytes: [u8; MAX_DATAGRAM],
}

type Inbox = Rc<RefCell<VecDeque<Packet>>>;

/// 仮の時計で動くネットワーク。2 つを `pair` で作り、互いの受信キューへ送る。送りに確保しない（受信キューは先に広げておく）。
pub struct FakeNet {
    me: SocketAddr,
    /// 相手のアドレス（これ以外の宛先へ送ったものは届かない。`misdirected` に数える）。
    peer: SocketAddr,
    /// 相手以外の宛先へ送った数（候補が 1 つの検査では 0 のはず）。
    pub misdirected: u32,
    clock: Rc<Cell<u64>>,
    inbox: Inbox,
    peer_inbox: Inbox,
    pub delay_us: u64,
    /// 送った n 通目（1 から）がこの倍数なら落とす（0 なら落とさない）。
    pub drop_every: u32,
    pub sent: u32,
    pub dropped: u32,
    pub waits: u32,
    /// 送った HGRT の 1 通を、ヘッダの type（offset 5）ごとに数えた数。対戦の成立の前に MENU・INPUT・CHECK などを送っていないことを見る。
    pub types: [u32; 32],
    /// `link_up`（接続の手順が成り立った知らせ）を受けた回数。本物は STUN の聞き直しをこれで止める。
    pub link_ups: u32,
    /// `own_port_varies` が返す値（2 か所目の STUN の比べの偽。既定は比べられていない `None`）。
    pub varies: Option<bool>,
    /// `own_nat` が返す UPnP の割り当ての有無（偽。既定は割り当てなし）。
    pub upnp: bool,
}

const INBOX_CAP: usize = 512;

impl FakeNet {
    pub fn pair(a: SocketAddr, b: SocketAddr, delay_us: u64) -> (FakeNet, FakeNet, Rc<Cell<u64>>) {
        let clock = Rc::new(Cell::new(1_000_000));
        let ia: Inbox = Rc::new(RefCell::new(VecDeque::with_capacity(INBOX_CAP)));
        let ib: Inbox = Rc::new(RefCell::new(VecDeque::with_capacity(INBOX_CAP)));
        let mk = |me, peer_addr, inbox: &Inbox, peer: &Inbox| FakeNet {
            me,
            peer: peer_addr,
            misdirected: 0,
            clock: Rc::clone(&clock),
            inbox: Rc::clone(inbox),
            peer_inbox: Rc::clone(peer),
            delay_us,
            drop_every: 0,
            sent: 0,
            dropped: 0,
            waits: 0,
            types: [0; 32],
            link_ups: 0,
            varies: None,
            upnp: false,
        };
        let na = mk(a, b, &ia, &ib);
        let nb = mk(b, a, &ib, &ia);
        (na, nb, clock)
    }

    /// 相手でない送り元 `from` から 1 通届いた形にする（今の時刻に着く。「候補に無い送り元」の検査用）。
    pub fn inject_from(&self, from: SocketAddr, bytes: &[u8]) {
        let mut p = Packet {
            due: self.clock.get(),
            from,
            len: bytes.len().min(MAX_DATAGRAM),
            bytes: [0; MAX_DATAGRAM],
        };
        if let Some(dst) = p.bytes.get_mut(..p.len) {
            dst.copy_from_slice(bytes.get(..p.len).unwrap_or(&[]));
        }
        self.inbox.borrow_mut().push_back(p);
    }

    fn next_due(&self) -> Option<u64> {
        self.inbox.borrow().front().map(|p| p.due)
    }
}

impl Outlet for FakeNet {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        if to != self.peer {
            self.misdirected += 1;
            return;
        }
        self.sent += 1;
        if bytes.starts_with(&higanr_core::wire::MAGIC)
            && let Some(n) = bytes
                .get(5)
                .and_then(|&t| self.types.get_mut(usize::from(t)))
        {
            *n += 1;
        }
        if self.drop_every != 0 && self.sent.is_multiple_of(self.drop_every) {
            self.dropped += 1;
            return;
        }
        let mut p = Packet {
            due: self.clock.get() + self.delay_us,
            from: self.me,
            len: bytes.len().min(MAX_DATAGRAM),
            bytes: [0; MAX_DATAGRAM],
        };
        if let Some(dst) = p.bytes.get_mut(..p.len) {
            dst.copy_from_slice(bytes.get(..p.len).unwrap_or(&[]));
        }
        let mut q = self.peer_inbox.borrow_mut();
        assert!(
            q.len() < INBOX_CAP,
            "仮のネットワークの受け箱が溢れた（検査の作りの誤り）"
        );
        q.push_back(p);
    }
}

impl Net for FakeNet {
    fn now_us(&mut self) -> u64 {
        self.clock.get()
    }

    fn recv(&mut self, buf: &mut [u8; MAX_DATAGRAM]) -> Option<(SocketAddr, usize, u64)> {
        let now = self.clock.get();
        let mut q = self.inbox.borrow_mut();
        if q.front().is_some_and(|p| p.due <= now) {
            let p = q.pop_front()?;
            buf.copy_from_slice(&p.bytes);
            return Some((p.from, p.len, p.due));
        }
        None
    }

    fn wait(&mut self, us: u32) -> bool {
        self.waits += 1;
        let now = self.clock.get();
        let until = now + u64::from(us);
        let (t, timed_out) = match self.next_due() {
            Some(d) if d <= until => (d.max(now), false),
            _ => (until, true),
        };
        self.clock.set(t);
        timed_out
    }

    fn link_up(&mut self) {
        self.link_ups += 1;
    }

    fn own_port_varies(&mut self) -> Option<bool> {
        self.varies
    }

    fn own_nat(&mut self) -> higanr_core::nat::NatMark {
        higanr_core::nat::NatMark {
            symmetry: higanr_core::nat::Symmetry::from_depends(self.varies),
            upnp: self.upnp,
        }
    }
}
