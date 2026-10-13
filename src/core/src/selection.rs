//! マッチの前の選択（難易度・キャラ・マップの画面）の状態機械。MENU・STEP_REQ・STEP・SELECT。
//! バイト列の形は `wire.rs`。ここは「いつ何を入れ、何を送るか」だけを持つ。
//!
//! 画面を移る押下はゲームの中で戻せない。順番を決めるのは step ごとにホスト 1 人にして、コピーが遅れて届いても
//! step の食い違いで捨てられるようにした。両者が対称に要求と了承をやり取りする形では、断られた要求のコピーが
//! 遅れて了承され、両者が同じ step に別の押下を入れた（`old_symmetric_protocol_diverges_on_note_scenario` が再現する）。
//!
//! dll が 1 フレームに呼ぶ順:
//! 1. 届いた MENU / STEP_REQ / STEP / SELECT を `on_menu` / `on_step_req` / `on_step` / `on_select` へ（届いた順）。
//! 2. `local_frame(自分の入力ワード（画面を移るビットを落とした後）, 画面を移る押下があれば Some(Press), 持っている押下を
//!    このフレームに入れられるか)`。3 つ目は `next_apply()` を見て dll が決める（画面がまだその押下の画面でない・入る演出の間なら false）。
//! 3. 返った `remote` と `apply` をゲームへ入れる。`apply` は受け入れたゲストの値のこともあるので、いつもカーソルを
//!    `apply.value` に合わせてから入れる。
//! 4. `apply` が選択を出る押下（難易度の画面の取消、マップの決定）なら、`build_menu` より先に `leave()`。マップの決定なら
//!    `set_local_select` もこのフレームで呼ぶ。出た後に送った MENU は step が次の選択と同じになっていて、遅れて次の選択に届くと
//!    相手が来ていないのに「来た」になり、ホストの押下が相手の居ない画面へ入るため。
//! 5. `build_menu()` が返したら送る（選択の外では返さない）。`poll_send(いまの ms)` が返すものを送る（フレームの外でも 1〜4 ms ごと。
//!    選択を出た後も、マッチの最初の INPUT が届くまで）。
//!
//! 押下の持ち越し: 画面を移る押下は両者のゲームへ同じ順に入るが、同じ時刻には入らない。入る演出の間はゲームが押下を読まない
//! ことがあり、ゲストのゲームだけが押下を読み捨てると画面が分かれる。そこでゲストは届いた STEP を、ホストは受けた要求を、
//! ゲームが読む状態になるまで持ち越す（`ready_ok`）。遅延の増分は、演出の間に届いた押下だけ演出が明けるまで。
//!
//! 合わせ: カーソルの動きは MENU の入力ワードの相対なので、最初のカーソルが違うと見た目がずっとずれる。両者が同じ画面にそろったら、
//! ホストが自分のカーソルの値を STEP（`StepKind::Align`）で送り、ゲストがその値へ書く。
//! - ホストは「相手が来ていて、前の STEP が届いていて、今の画面の合わせがまだ」（`align_due`）のとき、自分の押下より先に渡す。
//!   画面が移るたびに 1 回。同じ画面のままの押下（キャラ選択の 1 席の決定）の後には出さない。
//! - STEP なので step が進み、語の番号は 0 から数え直す。ゲストは合わせが届くまで動かず（`is_aligned`）、自分の押下も合わせの後に要求する。
//! - 合わせは画面を移らないので `Outcome` は `Stay`。取消では揃えない。
//! - ホストの合わせの tick は、自分の入力ワードに方向・Ctrl・Shift が立っていないときに選ぶ（dll が見る）。押し続けが跨ぐと、
//!   ゲスト側でその方向の「押した瞬間」が 1 回余計に数えられてカーソルが 1 つずれる。
//!
//! 相手の入力ワードの再生: 選択に入って最初に受けた MENU は、溜まった古い入力ワードを再生せず新しい入力ワードから始める（`fresh`。再生すると
//! 約 32 フレーム遅れ続ける）。合わせの後に溜まる「何も押していない」語は飛ばして追いつく（`next_remote`）。
//! 同じ画面に残る step（画面の模型 `menu_model::Model` の画面が変わらない決定・取消。合わせは除く）の後は、1 つ前の step の入力ワードの残りを
//! 新しい step の入力ワードより先に渡し、直前の入力ワードも 0 に戻さない。戻すと、相手が方向を押し続けているとき押した瞬間が 1 回余計にでき、
//! カーソルが 1 つずれる。送り手は押下を入れたフレームの自分の入力ワードを、新しい step の 0 番として積み直す。
//! 画面が移る step は古い step の入力ワードを捨てて直前の入力ワードを 0 に戻す。同じ画面に残るかは入れた STEP の列だけで決まり、
//! 模型と食い違ったら画面が移る step と同じに扱う。遅延の増分は 0。
//!
//! ゲームスレッドで呼ぶので、ヒープを使わず、パニックしうる添字を置かない。入力ワードの列は 32 ワードの固定のリングバッファで、
//! 添字はいつも `% 32`。数の足し算は飽和させる。

use crate::menu_model::{Model, Outcome};
use crate::wire::{MAX_WORDS, Menu, Message, Select, Step, StepKind};

/// STEP・STEP_REQ・SELECT を送り直す間隔（ms）。INPUT の待ち中の再送と同じ値に揃えてあるが、由来が別なので別の名前で持つ。
pub const RESEND_MS: u32 = 4;

/// MENU の入力ワードのリングの長さ。送り手は未 ack の新しい方から 32 ワードまで送り、受け手もそれ以上は溜めない（入力ワードはカーソルの表示にだけ効く）。
const RING: u32 = MAX_WORDS as u32;

/// 役割（role_nonce の大きい方がホスト）。lockstep と同じ型。
pub use crate::lockstep::Role;

/// 画面を移る押下。step はこの状態機械が付ける。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Press {
    pub kind: StepKind,
    /// 難易度・マップの画面ではカーソルの値、キャラ選択では押した席と Shift。ここでは意味を見ない。
    pub value: u8,
    pub screen: u8,
}

impl Press {
    fn at(self, step: u16) -> Step {
        Step {
            step,
            kind: self.kind,
            value: self.value,
            screen: self.screen,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameOut {
    /// 相手の入力ワード（届いた順に 1 フレーム 1 ワード。無ければ直前の入力ワード。画面が移る step の直後は 0）。
    pub remote: u16,
    /// このフレームで入れる画面を移る押下。`value` へカーソルを合わせてから入れる。
    /// `step` はこの押下の通し番号（ログ用。ゲームへは入れない）。
    pub apply: Option<Step>,
}

/// `poll_send` が返す、いま送るもの。送り直しの対象は同時に 2 つまであり得る（ホストの最後の STEP と SELECT）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outbox {
    pub step: Option<Message>,
    pub select: Option<Message>,
}

/// SELECT の値の食い違い（違えばログに残し、ホストの値を採る）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mismatch {
    pub map: bool,
    pub difficulty: bool,
    /// オプションの意味の分からない欄のダイジェスト（違えばログだけ）。
    pub option_digest: bool,
}

/// 両者の SELECT が揃ったときの値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agreed {
    /// 相手の SELECT そのまま（相手の席のキャラ・色・初期ライフ・チャージ型はここから書く）。
    pub peer: Select,
    /// マップ・難易度はホストの値（食い違いは `mismatch`）。
    pub map: u8,
    pub difficulty: u8,
    /// ホストだけが意味を持つ値。
    pub cpu_1p: u8,
    pub cpu_2p: u8,
    pub background: u8,
    pub rng_seed: u16,
    pub mismatch: Mismatch,
}

/// 捨てたものの数（ログ用）。増え続けるなら相手か経路がおかしい。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    /// 自分の画面を移る押下を入れなかった（相手が来ていない / 前の STEP が届いていない / 要求を持っている / 選択の外）。
    pub press_dropped: u32,
    /// ホストが捨てた STEP_REQ（違う step / 選択の外 / 同じフレームのホストの押下に負けた / 相手が来ていない）。
    pub req_dropped: u32,
    /// ゲストが捨てた STEP（古い = 重複 / 選択の外）。
    pub step_dropped: u32,
    /// 自分と違う step の MENU、選択の外で届いた MENU。
    pub menu_dropped: u32,
    /// match_no の違う SELECT、2 通目以降の SELECT。
    pub select_dropped: u32,
    /// 手順の上では来ないもの（ゲストの step がホストより先、新しすぎる STEP、役割の逆向きのメッセージ）。
    pub impossible: u32,
}

/// 選択の状態機械。接続ごとに 1 つ作り、選択に入るたびに `enter()`、出るたびに `leave()`。step と語の番号は接続の間持ち越す。
#[derive(Debug, Clone)]
pub struct Selection {
    role: Role,
    active: bool,
    /// 「相手が来た」: この選択に入ってから、自分と同じ step の相手の MENU が届いた。
    present: bool,
    /// 次に決まる押下の通し番号 = 入れ終えた押下の数。
    step: u16,
    /// 最後に入れた合わせの画面（この選択に入ってからの分。`enter` で消す）。今の画面と同じなら、その画面の合わせは済み。
    aligned: Option<u8>,
    /// 選択に入ってから、まだ相手の MENU（同じ step）を 1 通も受けていない（最初の MENU は古い入力ワードを再生しない）。
    fresh: bool,

    // ---- ホストだけが使う ----
    /// ゲストが入れ終えたと分かっている step（ゲストの MENU・STEP_REQ の step の最大。SELECT でも進む）。
    /// `guest_step >= step` が「前の STEP がゲストに届いた」。
    guest_step: u16,
    /// 受け入れ待ちの STEP_REQ（自分と同じ step のものだけ。次の `local_frame` で入れるか捨てる）。
    held_req: Option<Press>,
    /// 最後に決めた STEP（`guest_step` がこれを越えるまで送り直す）。
    last_step: Option<Step>,
    last_step_sent: Option<u32>,

    // ---- ゲストだけが使う ----
    /// 自分の要求（1 つだけ。step はいつも `self.step`。step が進んだら捨てる）。
    pending: Option<Press>,
    pending_sent: Option<u32>,
    /// 届いた、次に入れる STEP（`step == self.step` のものだけ）。
    ready: Option<Step>,

    // ---- MENU の入力ワード（番号は step ごとに 0 から） ----
    tx_ring: [u16; MAX_WORDS],
    tx_next: u32,
    /// 相手の ack（相手が次に要る自分の入力ワードの番号）。
    tx_acked: u32,
    /// 今の step の相手の入力ワード（`rx.end` が自分の送る ack）。
    rx: RxWords,
    /// 1 つ前の step の相手の入力ワードの残り（同じ画面に残る step の後だけ。モジュール doc の「同じ画面に残る step を
    /// またぐ相手の入力ワード」）。`prev_step` が Some の間だけ受け、`rx` より先に渡す。
    prev_rx: RxWords,
    /// `prev_rx` の step。新しい step の入力ワードを渡し始めたら・画面が移る step を入れたら・選択に入り直したら None。
    prev_step: Option<u16>,
    /// 直前にゲームへ渡した相手の入力ワード（次の入力ワードが無いフレームに続ける）。
    rx_last: u16,
    /// この選択に入ってから入れた STEP で追う画面（同じ画面に残る step かを決める）。
    screens: Screens,

    // ---- SELECT ----
    /// その接続で何戦目か（0 から。SELECT を置いた選択の次の `enter` で 1 進む）。
    match_no: u16,
    own_select: Option<Select>,
    /// 自分の SELECT を置いたときの step（ホスト: ゲストの SELECT が届いたら、ゲストはここまで入れ終えている）。
    own_select_step: u16,
    peer_select: Option<Select>,
    /// 相手の最初の INPUT が届いた（SELECT の送り直しをやめる）。
    peer_input_seen: bool,
    select_sent: Option<u32>,

    counters: Counters,
}

/// 相手の入力ワードの受け取りの列（1 つの step の分。番号は step ごとに 0 から）。32 ワードの固定のリングで持つ。
#[derive(Debug, Clone, Copy)]
struct RxWords {
    ring: [u16; MAX_WORDS],
    play: u32,
    /// 受け取った入力ワードの番号の終わり。`play..end` は全部リングにある（いつも `play <= end`）。
    end: u32,
}

impl RxWords {
    const EMPTY: Self = Self {
        ring: [0; MAX_WORDS],
        play: 0,
        end: 0,
    };

    fn len(&self) -> u32 {
        self.end - self.play
    }

    fn front(&self) -> Option<u16> {
        (self.play < self.end).then(|| self.ring[slot(self.play)])
    }

    fn pop(&mut self) -> Option<u16> {
        let w = self.front()?;
        self.play += 1;
        Some(w)
    }

    /// MENU の入力ワード（番号 `base` から）を番号の順に足す。番号が u32 を越える MENU は手順の外なので何もせず false。
    fn receive(&mut self, base: u32, words: &[u16]) -> bool {
        // 下の `base + i` が桁あふれしないことの確かめも兼ねる。
        let fits = u32::try_from(words.len())
            .ok()
            .and_then(|n| base.checked_add(n))
            .is_some();
        if !fits {
            return false;
        }
        if base > self.end {
            // 番号が飛んだ = 送り手が 32 ワードを越えて落とした。飛んだ先から続ける。
            self.play = base;
            self.end = base;
        }
        for (i, &w) in words.iter().enumerate() {
            let n = base + i as u32;
            if n == self.end {
                self.ring[slot(n)] = w;
                self.end += 1;
            }
        }
        if self.len() > RING {
            // 32 ワードより溜まった（片道の遅れが揺れて一度に届いた）。古い方を落とす。
            self.play = self.end - RING;
        }
        true
    }
}

/// この選択に入ってから入れた STEP で追う画面（`menu_model::Model`）。両者が同じ順に入れた STEP の列から同じ模型を進める。
#[derive(Debug, Clone, Copy)]
enum Screens {
    /// まだ STEP を入れていない（入った画面は席の旗が 0 —— `Model::new`）。
    Unseen,
    Known(Model),
    /// 入れた STEP が模型と食い違った。この選択の間は、どの step も画面が移るものとして扱う。
    Lost,
}

/// 前に送った時刻から `RESEND_MS` 経ったか。まだ送っていなければすぐ送る（遅延を足さないため）。
fn due(last: Option<u32>, now: u32) -> bool {
    match last {
        None => true,
        Some(t) => now.wrapping_sub(t) >= RESEND_MS,
    }
}

fn slot(n: u32) -> usize {
    (n % RING) as usize
}

impl Selection {
    pub fn new(role: Role) -> Self {
        Self {
            role,
            active: false,
            present: false,
            step: 0,
            aligned: None,
            fresh: false,
            guest_step: 0,
            held_req: None,
            last_step: None,
            last_step_sent: None,
            pending: None,
            pending_sent: None,
            ready: None,
            tx_ring: [0; MAX_WORDS],
            tx_next: 0,
            tx_acked: 0,
            rx: RxWords::EMPTY,
            prev_rx: RxWords::EMPTY,
            prev_step: None,
            rx_last: 0,
            screens: Screens::Unseen,
            match_no: 0,
            own_select: None,
            own_select_step: 0,
            peer_select: None,
            peer_input_seen: false,
            select_sent: None,
            counters: Counters::default(),
        }
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn step(&self) -> u16 {
        self.step
    }

    pub fn peer_present(&self) -> bool {
        self.present
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// ゲストの持っている要求。ホストはいつも None。
    pub fn pending_press(&self) -> Option<Press> {
        self.pending
    }

    pub fn match_no(&self) -> u16 {
        self.match_no
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// 選択に入る（Match の難易度画面、2 戦目以降はキャラ選択）。「相手が来た」を戻し、相手の入力ワードの溜まりは捨て、直前の入力ワードは 0 に戻す。
    /// ワードの番号は戻さない（相手は番号を続けて送るので、戻すと相手の入力ワードを古いと見て捨て続ける）。ホストの最後の STEP の送り直しは止めない。
    pub fn enter(&mut self) {
        if self.own_select.is_some() {
            // 前の選択で SELECT を置いた = 1 戦を始めた。両者の match_no は同じ所で進む。
            self.match_no = self.match_no.wrapping_add(1);
            self.own_select = None;
            self.peer_select = None;
            self.peer_input_seen = false;
            self.select_sent = None;
        }
        self.active = true;
        self.present = false;
        self.aligned = None;
        self.fresh = true;
        self.held_req = None;
        self.pending = None;
        self.pending_sent = None;
        self.ready = None;
        self.rx.play = self.rx.end;
        self.prev_step = None;
        self.rx_last = 0;
        self.screens = Screens::Unseen;
    }

    /// 今の画面 `screen` の合わせを入れ終えたか。ゲストはこれが偽の間、動かない。
    pub fn is_aligned(&self, screen: u8) -> bool {
        self.aligned == Some(screen)
    }

    /// ホスト: 今の画面 `screen` の合わせをいま押下として渡してよいか（相手が来ていて、前の STEP が届いていて、この画面の合わせがまだ）。
    /// ゲームが読む状態か・自分の入力ワードに方向が立っていないかは dll が見る。ゲストはいつも false。
    pub fn align_due(&self, screen: u8) -> bool {
        self.role == Role::Host
            && self.active
            && self.present
            && self.guest_step >= self.step
            && self.step < u16::MAX
            && self.aligned != Some(screen)
    }

    /// 選択を出る。自分の押下・ゲストの要求・STEP の受け入れをやめる。
    ///
    /// ホストの最後の STEP と SELECT の送り直しは止めない。選択を出る押下（難易度の画面の取消）の STEP が落ちたまま送り直しをやめると、
    /// ゲストだけが選択の画面に残って戻れない（`exit_step_lost_after_host_left_still_reaches_guest`）。
    /// STEP はゲストの step がそれを越えたら、SELECT は相手の最初の INPUT で止まる。
    pub fn leave(&mut self) {
        self.active = false;
        self.present = false;
        self.held_req = None;
        self.pending = None;
        self.pending_sent = None;
        self.ready = None;
    }

    /// 1 フレームぶん。`local` は画面を移るビットを落とした自分の入力ワード、`press` は画面を移る押下（ゲームがこのフレームに押下を読む
    /// 状態のときだけ渡す）。`ready_ok` は持っている押下をこのフレームにゲームへ入れられるか（false なら持ち越す）。
    /// このフレームの語は押下で画面が移る前の画面で使われるので、押下を入れる前の step に数える。
    pub fn local_frame(&mut self, local: u16, press: Option<Press>, ready_ok: bool) -> FrameOut {
        let remote = self.next_remote();
        self.tx_ring[slot(self.tx_next)] = local;
        self.tx_next = self.tx_next.saturating_add(1);

        let apply = match self.role {
            Role::Host => self.host_decide(press, ready_ok),
            Role::Guest => self.guest_take(press, ready_ok),
        };
        if let Some(st) = apply {
            let stays = self.stays_on_screen(&st);
            self.advance_step(stays);
            if stays {
                // 押下を入れたフレームの自分の入力ワードは同じ画面でカーソルを動かしている。前の step の MENU はもう作らないので、
                // 新しい step の 0 番に積み直す。
                self.tx_ring[slot(0)] = local;
                self.tx_next = 1;
            }
            if st.kind == StepKind::Align {
                self.aligned = Some(st.screen);
            }
        }
        FrameOut { remote, apply }
    }

    /// 入れた押下 `st` の後も同じ画面に残るか（相手の入力ワードを step をまたいで続けるか）を決め、画面の模型を進める。
    /// 合わせは false（合わせの値がそれより前の語を全部含むので、続けると二重に動く）。
    fn stays_on_screen(&mut self, st: &Step) -> bool {
        let mut model = match self.screens {
            Screens::Lost => return false,
            Screens::Unseen => Model::new(st.screen),
            Screens::Known(m) => m,
        };
        let before = model.screen();
        match model.apply(st) {
            Ok(outcome) => {
                self.screens = Screens::Known(model);
                outcome == Outcome::Stay && st.kind != StepKind::Align && model.screen() == before
            }
            Err(_) => {
                // 模型と食い違った。古い画面の語を新しい画面へ入れるより、捨てる側に倒す。
                self.screens = Screens::Lost;
                false
            }
        }
    }

    /// 次の `local_frame` が `ready_ok = true` なら入れる、持っている押下（ホスト: 受けたゲストの要求 / ゲスト: 届いた STEP）。
    /// dll はこれを見て、ゲームがその押下を読む状態かを決める。
    pub fn next_apply(&self) -> Option<Step> {
        match self.role {
            Role::Host => {
                let open = self.active
                    && self.present
                    && self.guest_step >= self.step
                    && self.step < u16::MAX;
                self.held_req.filter(|_| open).map(|r| r.at(self.step))
            }
            Role::Guest => self.ready.filter(|s| s.step == self.step),
        }
    }

    /// ホスト: このフレームで決める押下。自分の押下を先に見る（同じフレームにゲストの要求があってもホストを採る）。
    fn host_decide(&mut self, press: Option<Press>, ready_ok: bool) -> Option<Step> {
        let req = self.held_req.take();
        // 前の STEP がゲストに届いたか = ゲストの MENU か STEP_REQ の step がそれを越えた。
        let delivered = self.guest_step >= self.step;
        let open = self.active && self.present && delivered && self.step < u16::MAX;
        if !open {
            // 1 往復より短い間に画面を 2 回移る押下は人の手では出ないので捨てる。覚えておくと、ホストの画面が遅れて勝手に移る。
            if press.is_some() {
                self.counters.press_dropped = self.counters.press_dropped.saturating_add(1);
            }
            if req.is_some() {
                self.counters.req_dropped = self.counters.req_dropped.saturating_add(1);
            }
            return None;
        }
        let p = match (press, req) {
            (Some(p), Some(_)) => {
                // ゲストの要求は step の食い違いで捨てられる。
                self.counters.req_dropped = self.counters.req_dropped.saturating_add(1);
                p
            }
            (Some(p), None) => p,
            (None, Some(r)) if ready_ok => r,
            (None, Some(r)) => {
                // ゲームがまだ読まない（入る演出の間など）。同じ step のまま持ち越す。
                self.held_req = Some(r);
                return None;
            }
            (None, None) => return None,
        };
        let st = p.at(self.step);
        self.last_step = Some(st);
        self.last_step_sent = None;
        Some(st)
    }

    /// ゲスト: 自分の押下は入れずに要求にし、届いた STEP を step の順に入れる。
    fn guest_take(&mut self, press: Option<Press>, ready_ok: bool) -> Option<Step> {
        if let Some(p) = press {
            // 相手が来ていなければ要求しない（どちらも画面を移れない）。要求を持っている間の押下は捨てる。
            if self.active && self.present && self.pending.is_none() && self.step < u16::MAX {
                self.pending = Some(p);
                self.pending_sent = None;
            } else {
                self.counters.press_dropped = self.counters.press_dropped.saturating_add(1);
            }
        }
        if !ready_ok {
            // 届いた STEP は、ゲームが読む状態になるまで持ち越す。
            return None;
        }
        let st = self.ready.take()?;
        if st.step != self.step {
            // on_step で step を見てから置くので来ない。
            self.counters.impossible = self.counters.impossible.saturating_add(1);
            return None;
        }
        // 要求はここで捨てる。このフレームに押した分も移る前の画面で押したものなので捨てる。
        self.pending = None;
        self.pending_sent = None;
        Some(st)
    }

    /// step を 1 進め、ワードの番号を 0 から数え直す。`stays`（同じ画面に残る step）なら相手の入力ワードの残りを前の step の分として持ち、
    /// 直前の入力ワードも続ける。画面が移る step なら残りを捨てて直前の入力ワードを 0 に戻す。
    fn advance_step(&mut self, stays: bool) {
        if stays {
            // 前の step の残りをまだ持っていたら（新しい step の入力ワードを 1 つも渡さないうちにもう 1 つ来た）捨てる。
            self.prev_rx = self.rx;
            self.prev_step = Some(self.step);
        } else {
            self.prev_step = None;
            self.rx_last = 0;
        }
        self.step = self.step.saturating_add(1);
        self.fresh = false;
        self.tx_next = 0;
        self.tx_acked = 0;
        self.rx = RxWords::EMPTY;
    }

    /// 相手の入力ワードを 1 つ。次の入力ワードがまだ無ければ直前の入力ワードを続ける（0 にすると押し続けた方向が一瞬離れ、カーソルが余分に動く）。
    /// 前の step の残りがあれば先に渡す。
    fn next_remote(&mut self) -> u16 {
        // 語が溜まっているとき、「何も押していない」語が続く分は飛ばして追いつく（押した瞬間も押している長さも変わらない）。
        // 溜まりの原因の 1 つは、ゲストが合わせを入れる前の入る演出（8〜9 tick）の間にホストが送り続けた語。押している語は飛ばさない。
        while self.queued() > 1 && self.rx_last == 0 && self.front() == Some(0) {
            self.pop();
        }
        if let Some(w) = self.pop() {
            self.rx_last = w;
        }
        self.rx_last
    }

    /// 溜まっていてまだ渡していない相手の入力ワードの数（前の step の残りと今の step の分）。
    fn queued(&self) -> u32 {
        let prev = if self.prev_step.is_some() {
            self.prev_rx.len()
        } else {
            0
        };
        prev.saturating_add(self.rx.len())
    }

    fn front(&self) -> Option<u16> {
        match self.prev_rx.front() {
            Some(w) if self.prev_step.is_some() => Some(w),
            _ => self.rx.front(),
        }
    }

    fn pop(&mut self) -> Option<u16> {
        if self.prev_step.is_some()
            && let Some(w) = self.prev_rx.pop()
        {
            return Some(w);
        }
        let w = self.rx.pop()?;
        // 新しい step の入力ワードを渡し始めた。この後に前の step の入力ワードが遅れて届いても、渡すと順が入れ替わるので受けない。
        self.prev_step = None;
        Some(w)
    }

    /// 毎フレーム送る MENU。未 ack の新しい方から 32 ワードまで。選択の外では返さない。
    pub fn build_menu(&self) -> Option<Menu> {
        if !self.active {
            return None;
        }
        let oldest = self.tx_next.saturating_sub(RING);
        let base = self.tx_acked.max(oldest);
        let count = self.tx_next.saturating_sub(base).min(RING);
        let mut words = [0u16; MAX_WORDS];
        for (i, w) in words.iter_mut().enumerate().take(count as usize) {
            *w = self.tx_ring[slot(base.saturating_add(i as u32))];
        }
        Some(Menu {
            step: self.step,
            ack: self.rx.end,
            base,
            count: count as u8,
            words,
        })
    }

    /// ホスト: ゲストが step `s` にいると分かった。自分の step より大きい値は手順の外として数えない。
    fn note_guest_step(&mut self, s: u16) {
        if s > self.step {
            self.counters.impossible = self.counters.impossible.saturating_add(1);
        } else if s > self.guest_step {
            self.guest_step = s;
        }
    }

    /// 相手の MENU。自分と違う step のものは捨てる（ホストは ack として step だけ見る）。ただし同じ画面に残る step の後は、
    /// 1 つ前の step の MENU の入力ワードも残りとして受ける。
    pub fn on_menu(&mut self, m: &Menu) {
        if self.role == Role::Host {
            self.note_guest_step(m.step);
        }
        let count = (m.count as usize).min(MAX_WORDS);
        if self.active && self.prev_step == Some(m.step) {
            // 前の step の残り。ack は前の step の自分の入力ワードの番号なので見ない。「相手が来た」はもう立っている。
            if !self.prev_rx.receive(m.base, &m.words[..count]) {
                self.counters.impossible = self.counters.impossible.saturating_add(1);
            }
            return;
        }
        if !self.active || m.step != self.step {
            self.counters.menu_dropped = self.counters.menu_dropped.saturating_add(1);
            return;
        }
        self.present = true;
        let first = std::mem::take(&mut self.fresh);
        // 送っていない番号まで ack されることは無いが、来ても送った所で止める。
        let ack = m.ack.min(self.tx_next);
        if ack > self.tx_acked {
            self.tx_acked = ack;
        }
        if !self.rx.receive(m.base, &m.words[..count]) {
            self.counters.impossible = self.counters.impossible.saturating_add(1);
            return;
        }
        if first && self.rx.len() > 1 {
            // 選択に入って最初の MENU: 相手が入る前から溜めていた古い入力ワードは再生しない（再生すると以後の入力ワードもその分遅れる）。
            self.rx.play = self.rx.end - 1;
        }
    }

    /// ゲストの STEP_REQ（ホストだけが意味を持つ）。自分と同じ step の要求だけを、次の `local_frame` まで持つ。
    pub fn on_step_req(&mut self, r: &Step) {
        if self.role != Role::Host {
            self.counters.impossible = self.counters.impossible.saturating_add(1);
            return;
        }
        if r.kind == StepKind::Align {
            // 合わせを出すのはホストだけ（wire の decode も捨てる。ここは状態機械を直に呼ぶ経路の守り）。
            self.counters.impossible = self.counters.impossible.saturating_add(1);
            return;
        }
        self.note_guest_step(r.step);
        if !self.active || r.step != self.step {
            self.counters.req_dropped = self.counters.req_dropped.saturating_add(1);
            return;
        }
        if self.held_req.is_none() {
            self.held_req = Some(Press {
                kind: r.kind,
                value: r.value,
                screen: r.screen,
            });
        }
    }

    /// ホストの STEP（ゲストだけが意味を持つ）。自分の step と同じものだけを次の `local_frame` で入れる。
    /// 古いものは重複なので捨てる。新しすぎるものは来ない（ホストは前の STEP が届くまで次を決めない）。
    pub fn on_step(&mut self, s: &Step) {
        if self.role != Role::Guest || s.step > self.step {
            self.counters.impossible = self.counters.impossible.saturating_add(1);
            return;
        }
        if !self.active || s.step != self.step {
            self.counters.step_dropped = self.counters.step_dropped.saturating_add(1);
            return;
        }
        if self.ready.is_none() {
            self.ready = Some(*s);
        }
    }

    /// 自分の SELECT を置く（マップの決定を入れたフレームに呼ぶ）。ゲストはホストだけが意味を持つ欄を 0 にする。2 度目以降は無視する。
    pub fn set_local_select(&mut self, sel: &Select) {
        if self.own_select.is_some() {
            return;
        }
        let mut s = *sel;
        s.match_no = self.match_no;
        if self.role == Role::Guest {
            s.cpu_1p = 0;
            s.cpu_2p = 0;
            s.background = 0;
            s.rng_seed = 0;
        }
        self.own_select = Some(s);
        self.own_select_step = self.step;
        self.select_sent = None;
        self.select_acks_final_step();
    }

    /// 相手の SELECT。match_no の合うものの最初の 1 通だけ受ける。
    pub fn on_select(&mut self, s: &Select) {
        if s.match_no != self.match_no || self.peer_select.is_some() {
            self.counters.select_dropped = self.counters.select_dropped.saturating_add(1);
            return;
        }
        self.peer_select = Some(*s);
        self.select_acks_final_step();
    }

    /// ホスト: ゲストはマップの決定を入れてから SELECT を送るので、ゲストの SELECT は最後の STEP の ack を兼ねる。
    /// ゲストの MENU は選択を出た後は来ないので、これが無いと送り直しが止まらない。
    fn select_acks_final_step(&mut self) {
        if self.role == Role::Host && self.own_select.is_some() && self.peer_select.is_some() {
            let s = self.own_select_step.min(self.step);
            if s > self.guest_step {
                self.guest_step = s;
            }
        }
    }

    /// 相手の最初の INPUT が届いた。SELECT の送り直しをやめる。dll は、これから始まるマッチの seg の INPUT が届いたときだけ呼ぶこと
    /// （前のマッチの INPUT を数えると、相手に SELECT が届かないまま送り直しが止まる）。自分の SELECT を置く前の呼び出しは数えない。
    pub fn on_peer_input(&mut self) {
        if self.own_select.is_some() {
            self.peer_input_seen = true;
        }
    }

    pub fn selects_ready(&self) -> bool {
        self.own_select.is_some() && self.peer_select.is_some()
    }

    /// 揃った値。マップ・難易度・ホストだけの値はホストの SELECT から採り、食い違いに印を立てる。
    pub fn agreed(&self) -> Option<Agreed> {
        let own = self.own_select?;
        let peer = self.peer_select?;
        let (host, guest) = match self.role {
            Role::Host => (own, peer),
            Role::Guest => (peer, own),
        };
        Some(Agreed {
            peer,
            map: host.map,
            difficulty: host.difficulty,
            cpu_1p: host.cpu_1p,
            cpu_2p: host.cpu_2p,
            background: host.background,
            rng_seed: host.rng_seed,
            mismatch: Mismatch {
                map: host.map != guest.map,
                difficulty: host.difficulty != guest.difficulty,
                option_digest: host.option_digest != guest.option_digest,
            },
        })
    }

    /// いま送るもの（新しいものはすぐ、送り直しは `RESEND_MS` ごと）。
    pub fn poll_send(&mut self, now: u32) -> Outbox {
        let mut out = Outbox::default();
        match self.role {
            Role::Host => {
                if let Some(st) = self.last_step
                    && self.guest_step <= st.step
                    && due(self.last_step_sent, now)
                {
                    out.step = Some(Message::Step(st));
                    self.last_step_sent = Some(now);
                }
            }
            Role::Guest => {
                if let Some(p) = self.pending
                    && self.active
                    && due(self.pending_sent, now)
                {
                    out.step = Some(Message::StepReq(p.at(self.step)));
                    self.pending_sent = Some(now);
                }
            }
        }
        if let Some(sel) = self.own_select
            && !self.peer_input_seen
            && due(self.select_sent, now)
        {
            out.select = Some(Message::Select(sel));
            self.select_sent = Some(now);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    //! 部品ごとの検査と、2 台の模型（1 ms 刻みの時計・方向ごとの送達の列・約 16 ms ごとのフレーム・4 ms ごとの送り直し）の検査。
    //! 2 台の模型の合格の不変量は `check_logs`。

    use super::*;
    use std::collections::HashMap;

    type Pr = Option<(StepKind, u8)>;
    type Entry = (u16, StepKind, u8);
    type FaultFn<'a> = dyn FnMut(Ctx) -> Vec<u32> + 'a;
    type PressFn<'a> = dyn FnMut(Role, u32, u16) -> Pr + 'a;

    const D: StepKind = StepKind::Decide;
    const C: StepKind = StepKind::Cancel;

    fn pr(kind: StepKind, value: u8) -> Option<Press> {
        Some(Press {
            kind,
            value,
            screen: 0,
        })
    }

    fn scr_press(kind: StepKind, value: u8, screen: u8) -> Option<Press> {
        Some(Press {
            kind,
            value,
            screen,
        })
    }

    fn to_press(p: Pr) -> Option<Press> {
        p.and_then(|(k, v)| pr(k, v))
    }

    /// 検査用の決まった乱数（splitmix64）。
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u32) -> u32 {
            (self.next() % u64::from(n)) as u32
        }
        fn chance(&mut self, pct: u32) -> bool {
            self.below(100) < pct
        }
    }

    // ================= 2 台の模型 =================

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum Dir {
        H2G,
        G2H,
    }

    /// 送達の筋書きが見る種類。旧バージョンの模型では要求が Req、了承 / 断りが Step。
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum Kind {
        Menu,
        Req,
        Step,
        Select,
        Other,
    }

    /// 送達の筋書きに渡すもの。`nth` は（向き・種類・step）ごとの何通目か（0 から）。
    #[derive(Clone, Copy, Debug)]
    struct Ctx {
        now: u32,
        dir: Dir,
        kind: Kind,
        step: u16,
        nth: u32,
    }

    /// 片方の機械。新しい手順（`NewNode`）と旧バージョンのコピー（`OldNode`）を同じ模型で回すための形。
    trait Node {
        type Msg: Clone;
        fn frame(&mut self, now: u32, press: Pr, out: &mut Vec<Self::Msg>);
        fn recv(&mut self, now: u32, m: Self::Msg, out: &mut Vec<Self::Msg>);
        fn tick(&mut self, now: u32, out: &mut Vec<Self::Msg>);
        fn cur_step(&self) -> u16;
        fn label(m: &Self::Msg) -> (Kind, u16);
    }

    struct Cfg {
        end: u32,
        host_phase: u32,
        host_period: u32,
        guest_phase: u32,
        guest_period: u32,
    }

    /// 決め打ちの筋の既定: ホストのフレームは 0, 16, 32, …、ゲストは 8, 24, 40, …。片道 10 ms。
    const CFG: Cfg = Cfg {
        end: 1000,
        host_phase: 0,
        host_period: 16,
        guest_phase: 8,
        guest_period: 16,
    };

    /// 方向ごとの送達の列。筋書きが返す遅れの列の数だけ届く（空 = 落とす、2 つ = 重ねる、
    /// 遅れの違い = 入れ替える）。
    struct Net<M> {
        q: Vec<(u32, u64, Dir, M)>,
        seq: u64,
        nth: HashMap<(Dir, Kind, u16), u32>,
    }

    impl<M: Clone> Net<M> {
        fn send(
            &mut self,
            now: u32,
            dir: Dir,
            msgs: &mut Vec<M>,
            label: fn(&M) -> (Kind, u16),
            faults: &mut FaultFn<'_>,
        ) {
            for m in msgs.drain(..) {
                let (kind, step) = label(&m);
                let n = self.nth.entry((dir, kind, step)).or_insert(0);
                let ctx = Ctx {
                    now,
                    dir,
                    kind,
                    step,
                    nth: *n,
                };
                *n += 1;
                for d in faults(ctx) {
                    self.q.push((now + d.max(1), self.seq, dir, m.clone()));
                    self.seq += 1;
                }
            }
        }

        fn take_due(&mut self, now: u32) -> Vec<(Dir, M)> {
            let mut due = Vec::new();
            let mut i = 0;
            while i < self.q.len() {
                if self.q[i].0 <= now {
                    due.push(self.q.swap_remove(i));
                } else {
                    i += 1;
                }
            }
            due.sort_by_key(|x| (x.0, x.1));
            due.into_iter().map(|x| (x.2, x.3)).collect()
        }
    }

    /// 1 ms ずつ: 届いたものを渡す → フレームの来た側が 1 フレーム進む → 両者が送り直しを見る。
    fn run<N: Node>(
        h: &mut N,
        g: &mut N,
        cfg: &Cfg,
        faults: &mut FaultFn<'_>,
        presses: &mut PressFn<'_>,
    ) {
        let mut net: Net<N::Msg> = Net {
            q: Vec::new(),
            seq: 0,
            nth: HashMap::new(),
        };
        let mut out = Vec::new();
        for t in 0..cfg.end {
            for (dir, m) in net.take_due(t) {
                match dir {
                    Dir::H2G => {
                        g.recv(t, m, &mut out);
                        net.send(t, Dir::G2H, &mut out, N::label, faults);
                    }
                    Dir::G2H => {
                        h.recv(t, m, &mut out);
                        net.send(t, Dir::H2G, &mut out, N::label, faults);
                    }
                }
            }
            if t >= cfg.host_phase && (t - cfg.host_phase).is_multiple_of(cfg.host_period) {
                let p = presses(Role::Host, t, h.cur_step());
                h.frame(t, p, &mut out);
                net.send(t, Dir::H2G, &mut out, N::label, faults);
            }
            if t >= cfg.guest_phase && (t - cfg.guest_phase).is_multiple_of(cfg.guest_period) {
                let p = presses(Role::Guest, t, g.cur_step());
                g.frame(t, p, &mut out);
                net.send(t, Dir::G2H, &mut out, N::label, faults);
            }
            h.tick(t, &mut out);
            net.send(t, Dir::H2G, &mut out, N::label, faults);
            g.tick(t, &mut out);
            net.send(t, Dir::G2H, &mut out, N::label, faults);
        }
    }

    fn lossless(_: Ctx) -> Vec<u32> {
        vec![10]
    }

    /// 合格の不変量。step が 0 から抜けも重なりも無く並び、ゲストの列はホストの列の先頭と一致する。
    /// `final_eq` なら全部一致する。
    fn check_logs(h: &[Entry], g: &[Entry], final_eq: bool) -> Result<(), String> {
        for (who, log) in [("host", h), ("guest", g)] {
            for (i, e) in log.iter().enumerate() {
                if usize::from(e.0) != i {
                    return Err(format!("{who} の {i} 番目の step が {}: {log:?}", e.0));
                }
            }
        }
        if g.len() > h.len() || h[..g.len()] != *g {
            return Err(format!(
                "ゲストの列がホストの列の先頭と違う: host={h:?} guest={g:?}"
            ));
        }
        if final_eq && h != g {
            return Err(format!("最後に揃っていない: host={h:?} guest={g:?}"));
        }
        Ok(())
    }

    /// 検査の筋で「選択を出る押下」とみなすもの（取消で value の下 2 ビットが 3）。
    fn is_exit(st: &Step) -> bool {
        st.kind == StepKind::Cancel && st.value % 4 == 3
    }

    /// 新しい手順の 1 台。dll の呼ぶ順（モジュール doc）で呼ぶ。
    struct NewNode {
        sel: Selection,
        log: Vec<Entry>,
        frame_no: u32,
        exits: bool,
        reenter_at: Option<u32>,
        rng: Rng,
        /// (step, ワード)。自分が積んだ入力ワードと、ゲームへ渡した相手の入力ワード。
        sent_words: Vec<(u16, u16)>,
        got_words: Vec<(u16, u16)>,
        reqs_sent: Vec<Entry>,
        steps_sent: Vec<(u32, Entry)>,
        applied_at: Vec<u32>,
        exits_done: u32,
        /// 押下を入れた後の「入る演出」の長さの上限（フレーム。0 なら演出なし）。演出の間はゲームが押下を読まない模型。
        busy_max: u32,
        busy_left: u32,
        /// 演出の間に、持っている押下を持ち越したフレームの数。
        held_frames: u32,
        /// 演出の間に押下が入ってしまった数（0 でなければ誤り）。
        applied_while_busy: u32,
    }

    impl NewNode {
        fn new(role: Role, enter_at: u32, exits: bool, seed: u64) -> Self {
            Self {
                sel: Selection::new(role),
                log: Vec::new(),
                frame_no: 0,
                exits,
                reenter_at: Some(enter_at),
                rng: Rng(seed),
                sent_words: Vec::new(),
                got_words: Vec::new(),
                reqs_sent: Vec::new(),
                steps_sent: Vec::new(),
                applied_at: Vec::new(),
                exits_done: 0,
                busy_max: 0,
                busy_left: 0,
                held_frames: 0,
                applied_while_busy: 0,
            }
        }
    }

    impl Node for NewNode {
        type Msg = Message;

        fn frame(&mut self, now: u32, press: Pr, out: &mut Vec<Message>) {
            if !self.sel.is_active() {
                match self.reenter_at {
                    Some(at) if now >= at => {
                        self.sel.enter();
                        self.reenter_at = None;
                    }
                    _ => return,
                }
            }
            self.frame_no += 1;
            // 語は送り手ごとに一意で 0 にならない（受け手が並びと重複を確かめられるように）。
            let word = 0x8000 | (self.frame_no & 0x7FFF) as u16;
            let s = self.sel.step();
            let busy = self.busy_left > 0;
            self.busy_left = self.busy_left.saturating_sub(1);
            // 演出の間は、ゲームが押下を読まないので、自分の押下も押下にしない。
            let press = if busy { None } else { press };
            if busy && self.sel.next_apply().is_some() {
                self.held_frames += 1;
            }
            let fo = self.sel.local_frame(word, to_press(press), !busy);
            self.sent_words.push((s, word));
            self.got_words.push((s, fo.remote));
            if let Some(st) = fo.apply {
                if busy {
                    self.applied_while_busy += 1;
                }
                if self.busy_max > 0 {
                    self.busy_left = 1 + self.rng.below(self.busy_max);
                }
                self.log.push((st.step, st.kind, st.value));
                self.applied_at.push(now);
                if self.exits && is_exit(&st) {
                    // 選択を出る押下を入れたフレームは、MENU を作る前に leave。
                    self.sel.leave();
                    self.exits_done += 1;
                    self.reenter_at = Some(now + 30 + self.rng.below(300));
                }
            }
            if let Some(m) = self.sel.build_menu() {
                out.push(Message::Menu(m));
            }
        }

        fn recv(&mut self, _now: u32, m: Message, _out: &mut Vec<Message>) {
            match m {
                Message::Menu(x) => self.sel.on_menu(&x),
                Message::StepReq(x) => self.sel.on_step_req(&x),
                Message::Step(x) => self.sel.on_step(&x),
                Message::Select(x) => self.sel.on_select(&x),
                _ => {}
            }
        }

        fn tick(&mut self, now: u32, out: &mut Vec<Message>) {
            let o = self.sel.poll_send(now);
            if let Some(m) = o.step {
                match m {
                    Message::StepReq(r) => self.reqs_sent.push((r.step, r.kind, r.value)),
                    Message::Step(s) => self.steps_sent.push((now, (s.step, s.kind, s.value))),
                    _ => {}
                }
                out.push(m);
            }
            if let Some(m) = o.select {
                out.push(m);
            }
        }

        fn cur_step(&self) -> u16 {
            self.sel.step()
        }

        fn label(m: &Message) -> (Kind, u16) {
            match m {
                Message::Menu(x) => (Kind::Menu, x.step),
                Message::StepReq(x) => (Kind::Req, x.step),
                Message::Step(x) => (Kind::Step, x.step),
                Message::Select(x) => (Kind::Select, x.match_no),
                _ => (Kind::Other, 0),
            }
        }
    }

    /// 受け手がゲームへ渡した相手の入力ワード（直前の入力ワードの繰り返しと 0 を除く）が、step ごとに、
    /// 送り手の積んだ入力ワードの部分列になっている（順が入れ替わらない・作られない）。確かめた入力ワードの数を返す。
    fn words_ok(sent: &[(u16, u16)], got: &[(u16, u16)]) -> Result<usize, String> {
        let mut by_step: HashMap<u16, Vec<u16>> = HashMap::new();
        for &(s, w) in sent {
            by_step.entry(s).or_default().push(w);
        }
        let mut pos: HashMap<u16, usize> = HashMap::new();
        let mut prev = None;
        let mut n = 0;
        for &(s, w) in got {
            if w == 0 || prev == Some((s, w)) {
                prev = Some((s, w));
                continue;
            }
            prev = Some((s, w));
            let list = by_step.get(&s).map(Vec::as_slice).unwrap_or(&[]);
            let p = pos.entry(s).or_insert(0);
            match list[*p..].iter().position(|&x| x == w) {
                Some(i) => *p += i + 1,
                None => {
                    return Err(format!(
                        "step {s} の語 {w:#06x} が送り手の並びに無い / 順が戻った"
                    ));
                }
            }
            n += 1;
        }
        Ok(n)
    }

    // ================= 旧バージョンのコピー（対称の了承制） =================

    /// 旧バージョンの手順を再現した模型: 要求した側は了承を受けて入れる。受けた側は了承を返して入れる。了承待ちの間に同じ step の
    /// 要求を受けたら、ホストは断り、ゲストは取り下げて了承する。違う step は断る。書いていない所は旧バージョンに有利に読んだ。
    #[derive(Clone, Debug)]
    enum OldMsg {
        Req(u16, StepKind, u8),
        Ack(u16, bool),
    }

    struct OldNode {
        host: bool,
        step: u16,
        log: Vec<Entry>,
        pending: Option<(StepKind, u8)>,
        sent: Option<u32>,
    }

    impl OldNode {
        fn new(host: bool) -> Self {
            Self {
                host,
                step: 0,
                log: Vec::new(),
                pending: None,
                sent: None,
            }
        }
        fn apply(&mut self, k: StepKind, v: u8) {
            self.log.push((self.step, k, v));
            self.step += 1;
        }
    }

    impl Node for OldNode {
        type Msg = OldMsg;

        fn frame(&mut self, _now: u32, press: Pr, _out: &mut Vec<OldMsg>) {
            if let Some(p) = press
                && self.pending.is_none()
            {
                self.pending = Some(p);
                self.sent = None;
            }
        }

        fn recv(&mut self, _now: u32, m: OldMsg, out: &mut Vec<OldMsg>) {
            match m {
                OldMsg::Req(s, k, v) => {
                    let accept_now = s == self.step && !(self.pending.is_some() && self.host);
                    if s < self.step {
                        let same = self.log.get(usize::from(s)) == Some(&(s, k, v));
                        out.push(OldMsg::Ack(s, same));
                    } else if accept_now {
                        self.pending = None;
                        self.apply(k, v);
                        out.push(OldMsg::Ack(s, true));
                    } else {
                        out.push(OldMsg::Ack(s, false));
                    }
                }
                OldMsg::Ack(s, ok) => {
                    if s == self.step
                        && let Some((k, v)) = self.pending.take()
                        && ok
                    {
                        self.apply(k, v);
                    }
                }
            }
        }

        fn tick(&mut self, now: u32, out: &mut Vec<OldMsg>) {
            if let Some((k, v)) = self.pending
                && due(self.sent, now)
            {
                out.push(OldMsg::Req(self.step, k, v));
                self.sent = Some(now);
            }
        }

        fn cur_step(&self) -> u16 {
            self.step
        }

        fn label(m: &OldMsg) -> (Kind, u16) {
            match m {
                OldMsg::Req(s, ..) => (Kind::Req, *s),
                OldMsg::Ack(s, _) => (Kind::Step, *s),
            }
        }
    }

    // ================= 旧バージョンの筋 =================

    #[test]
    fn old_symmetric_protocol_diverges_on_note_scenario() {
        // ホストの P のコピーの 1 通だけが遅れ、step の追いついたゲストが Q を了承待ちで持つ時刻に届く。
        let mut h = OldNode::new(true);
        let mut g = OldNode::new(false);
        let cfg = Cfg {
            end: 400,
            guest_phase: 0,
            ..CFG
        };
        let mut faults = |c: Ctx| match (c.dir, c.kind, c.step, c.nth) {
            (Dir::H2G, Kind::Step, 0, _) => vec![50],
            (Dir::H2G, Kind::Req, 1, 4) => vec![68],
            (Dir::H2G, Kind::Step, 1, _) => vec![40],
            _ => vec![10],
        };
        let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
            (Role::Guest, 0) => Some((D, 4)),
            (Role::Host, 16) => Some((D, 1)),
            (Role::Guest, 64) => Some((C, 5)),
            _ => None,
        };
        run(&mut h, &mut g, &cfg, &mut faults, &mut presses);
        assert_eq!(
            h.log,
            vec![(0, D, 4), (1, C, 5)],
            "ホストはゲストの Q を入れる"
        );
        assert_eq!(
            g.log,
            vec![(0, D, 4), (1, D, 1)],
            "ゲストは遅れて届いた P を入れる"
        );
        assert!(check_logs(&h.log, &g.log, true).is_err());
    }

    #[test]
    fn new_protocol_agrees_on_note_scenario_lagging_guest() {
        let mut h = NewNode::new(Role::Host, 0, false, 1);
        let mut g = NewNode::new(Role::Guest, 0, false, 2);
        let mut faults = |c: Ctx| match (c.dir, c.kind, c.step, c.nth) {
            (Dir::H2G, Kind::Step, 0, _) => vec![50],
            (Dir::G2H, Kind::Req, 0, n) if n >= 3 => vec![150],
            _ => vec![10],
        };
        let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
            (Role::Guest, 40) => Some((D, 4)),
            (Role::Host, 80 | 96 | 112) => Some((D, 1)),
            (Role::Guest, 136) => Some((C, 5)),
            _ => None,
        };
        run(&mut h, &mut g, &CFG, &mut faults, &mut presses);
        assert_eq!(h.log, vec![(0, D, 4), (1, C, 5)]);
        check_logs(&h.log, &g.log, true).unwrap();
        assert_eq!(h.sel.counters().press_dropped, 3, "P は 3 回とも捨てる");
        assert!(
            h.steps_sent.iter().all(|&(_, e)| e.2 != 1),
            "P の STEP は 1 通も出ない: {:?}",
            h.steps_sent
        );
        assert!(
            h.sel.counters().req_dropped > 0,
            "遅れた R0 のコピーが届いて捨てられた（効く期間）"
        );
    }

    #[test]
    fn new_protocol_agrees_on_note_scenario_stale_request_copy() {
        // Q のコピーの 1 通だけが遅れ、ゲストが step 1 で R を要求してホストが R を持っている時刻に届く。
        let mut h = NewNode::new(Role::Host, 0, false, 1);
        let mut g = NewNode::new(Role::Guest, 0, false, 2);
        let mut faults = |c: Ctx| match (c.dir, c.kind, c.step, c.nth) {
            (Dir::G2H, Kind::Req, 0, 5) => vec![60],
            _ => vec![10],
        };
        let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
            (Role::Guest, 40) => Some((C, 5)),
            (Role::Host, 64) => Some((D, 1)),
            (Role::Guest, 104) => Some((D, 6)),
            _ => None,
        };
        run(&mut h, &mut g, &CFG, &mut faults, &mut presses);
        assert_eq!(
            h.log,
            vec![(0, D, 1), (1, D, 6)],
            "step 0 はホスト、step 1 はゲストの R"
        );
        check_logs(&h.log, &g.log, true).unwrap();
        assert!(
            g.reqs_sent.contains(&(0, C, 5)),
            "Q は要求された（効く期間）"
        );
        assert!(
            g.reqs_sent.contains(&(1, D, 6)),
            "R は要求された（効く期間）"
        );
    }

    // ================= 決め打ちの筋 =================

    #[test]
    fn host_only_presses() {
        let mut h = NewNode::new(Role::Host, 0, false, 1);
        let mut g = NewNode::new(Role::Guest, 0, false, 2);
        let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
            (Role::Host, 32) => Some((D, 0)),
            (Role::Host, 96) => Some((C, 1)),
            (Role::Host, 160) => Some((D, 2)),
            _ => None,
        };
        run(&mut h, &mut g, &CFG, &mut lossless, &mut presses);
        assert_eq!(h.log, vec![(0, D, 0), (1, C, 1), (2, D, 2)]);
        check_logs(&h.log, &g.log, true).unwrap();
        assert_eq!(h.applied_at, vec![32, 96, 160]);
    }

    #[test]
    fn guest_only_presses() {
        let mut h = NewNode::new(Role::Host, 0, false, 1);
        let mut g = NewNode::new(Role::Guest, 0, false, 2);
        let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
            (Role::Guest, 40) => Some((D, 4)),
            (Role::Guest, 200) => Some((C, 5)),
            _ => None,
        };
        run(&mut h, &mut g, &CFG, &mut lossless, &mut presses);
        assert_eq!(h.log, vec![(0, D, 4), (1, C, 5)]);
        check_logs(&h.log, &g.log, true).unwrap();
        assert_eq!(h.applied_at[0], 64);
        assert_eq!(g.applied_at[0], 88);
    }

    #[test]
    fn same_frame_press_host_wins_decide_and_cancel_overlap() {
        for (hp, gp) in [((D, 1), (C, 5)), ((C, 2), (D, 6))] {
            let mut h = NewNode::new(Role::Host, 0, false, 1);
            let mut g = NewNode::new(Role::Guest, 0, false, 2);
            let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
                (Role::Guest, 40) => Some(gp),
                (Role::Host, 64) => Some(hp),
                _ => None,
            };
            run(&mut h, &mut g, &CFG, &mut lossless, &mut presses);
            assert_eq!(
                h.log,
                vec![(0, hp.0, hp.1)],
                "ホストを採り、ゲストの要求は入らない"
            );
            check_logs(&h.log, &g.log, true).unwrap();
            assert!(h.sel.counters().req_dropped >= 1);
            assert_eq!(
                g.sel.pending_press(),
                None,
                "ゲストは STEP を入れて要求を捨てた"
            );
        }
    }

    #[test]
    fn step_req_loss_dup_reorder() {
        let mut h = NewNode::new(Role::Host, 0, false, 1);
        let mut g = NewNode::new(Role::Guest, 0, false, 2);
        let mut faults = |c: Ctx| match (c.dir, c.kind, c.nth) {
            (Dir::G2H, Kind::Req, 0..=2) => vec![],
            (Dir::G2H, Kind::Req, 3) => vec![30, 5],
            (Dir::G2H, Kind::Req, n) => vec![3 + (n % 3) * 9],
            _ => vec![10],
        };
        let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
            (Role::Guest, 40) => Some((D, 4)),
            (Role::Guest, 248) => Some((C, 7)),
            _ => None,
        };
        run(&mut h, &mut g, &CFG, &mut faults, &mut presses);
        assert_eq!(h.log, vec![(0, D, 4), (1, C, 7)]);
        check_logs(&h.log, &g.log, true).unwrap();
        assert!(
            h.sel.counters().req_dropped > 0,
            "重複のコピーが届いて捨てられた（効く期間）"
        );
    }

    #[test]
    fn step_loss_dup_reorder() {
        let mut h = NewNode::new(Role::Host, 0, false, 1);
        let mut g = NewNode::new(Role::Guest, 0, false, 2);
        let mut faults = |c: Ctx| match (c.dir, c.kind, c.step, c.nth) {
            (Dir::H2G, Kind::Step, 0, 0..=3) => vec![],
            (Dir::H2G, Kind::Step, 0, 4) => vec![3, 40],
            (Dir::H2G, Kind::Step, 1, n) => vec![30 - (n % 4) * 7, 12],
            _ => vec![10],
        };
        let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
            (Role::Host, 32) => Some((D, 0)),
            (Role::Host, 240) => Some((D, 3)),
            _ => None,
        };
        run(&mut h, &mut g, &CFG, &mut faults, &mut presses);
        assert_eq!(h.log, vec![(0, D, 0), (1, D, 3)]);
        check_logs(&h.log, &g.log, true).unwrap();
        assert!(
            g.sel.counters().step_dropped > 0,
            "重複の STEP が届いて捨てられた（効く期間）"
        );
    }

    #[test]
    fn stale_step_and_menu_are_ignored() {
        let mut h = NewNode::new(Role::Host, 0, false, 1);
        let mut g = NewNode::new(Role::Guest, 0, false, 2);
        let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
            (Role::Host, 32) => Some((D, 0)),
            (Role::Host, 96) => Some((D, 1)),
            (Role::Host, 160) => Some((D, 2)),
            _ => None,
        };
        run(&mut h, &mut g, &CFG, &mut lossless, &mut presses);
        assert_eq!(g.sel.step(), 3);
        let before = g.log.clone();
        let dropped = g.sel.counters().step_dropped;
        g.sel.on_step(&Step {
            step: 0,
            kind: C,
            value: 3,
            screen: 0,
        });
        let old_menu = menu(1, 0, 0, &[0x1234]);
        let menu_dropped = g.sel.counters().menu_dropped;
        g.sel.on_menu(&old_menu);
        let fo = g.sel.local_frame(0, None, true);
        assert_eq!(fo.apply, None);
        assert_ne!(fo.remote, 0x1234, "古い step の語はゲームへ渡さない");
        assert_eq!(g.sel.step(), 3);
        assert_eq!(g.log, before);
        assert_eq!(g.sel.counters().step_dropped, dropped + 1);
        assert_eq!(g.sel.counters().menu_dropped, menu_dropped + 1);
        assert_eq!(h.sel.guest_step, 3);
        h.sel.on_menu(&old_menu);
        h.sel.on_step_req(&Step {
            step: 1,
            kind: D,
            value: 4,
            screen: 0,
        });
        assert_eq!(h.sel.guest_step, 3);
        assert_eq!(h.sel.local_frame(0, None, true).apply, None);
    }

    #[test]
    fn press_before_peer_arrives_is_dropped() {
        let mut h = NewNode::new(Role::Host, 0, false, 1);
        let mut g = NewNode::new(Role::Guest, 300, false, 2);
        let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
            (Role::Host, 32 | 128 | 288) => Some((D, 0)),
            (Role::Host, 352) => Some((C, 1)),
            _ => None,
        };
        run(&mut h, &mut g, &CFG, &mut lossless, &mut presses);
        assert_eq!(h.log, vec![(0, C, 1)]);
        assert_eq!(h.sel.counters().press_dropped, 3);
        check_logs(&h.log, &g.log, true).unwrap();

        let mut h = NewNode::new(Role::Host, 300, false, 1);
        let mut g = NewNode::new(Role::Guest, 0, false, 2);
        let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
            (Role::Guest, 40) => Some((D, 4)),
            (Role::Guest, 360) => Some((D, 5)),
            _ => None,
        };
        run(&mut h, &mut g, &CFG, &mut lossless, &mut presses);
        assert!(
            g.reqs_sent.iter().all(|e| e.2 == 5),
            "来る前の押下は要求しない: {:?}",
            g.reqs_sent
        );
        assert_eq!(h.log, vec![(0, D, 5)]);
        check_logs(&h.log, &g.log, true).unwrap();
    }

    #[test]
    fn host_press_before_prev_step_delivered_is_dropped() {
        let mut h = NewNode::new(Role::Host, 0, false, 1);
        let mut g = NewNode::new(Role::Guest, 0, false, 2);
        let mut faults = |c: Ctx| match (c.dir, c.kind, c.step) {
            (Dir::H2G, Kind::Step, 0) => vec![100],
            _ => vec![10],
        };
        let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
            (Role::Host, 32) => Some((D, 0)),
            (Role::Host, 48 | 80 | 144) => Some((D, 1)),
            (Role::Host, 160) => Some((C, 2)),
            _ => None,
        };
        run(&mut h, &mut g, &CFG, &mut faults, &mut presses);
        assert_eq!(h.log, vec![(0, D, 0), (1, C, 2)]);
        assert_eq!(h.sel.counters().press_dropped, 3);
        check_logs(&h.log, &g.log, true).unwrap();
    }

    #[test]
    fn exit_step_lost_after_host_left_still_reaches_guest() {
        let mut h = NewNode::new(Role::Host, 0, true, 1);
        let mut g = NewNode::new(Role::Guest, 0, true, 2);
        let mut faults = |c: Ctx| match (c.dir, c.kind, c.nth) {
            (Dir::H2G, Kind::Step, 0..=9) => vec![],
            _ => vec![10],
        };
        let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
            (Role::Host, 32) => Some((C, 3)),
            _ => None,
        };
        let cfg = Cfg { end: 200, ..CFG };
        run(&mut h, &mut g, &cfg, &mut faults, &mut presses);
        assert_eq!(h.log, vec![(0, C, 3)]);
        assert_eq!(g.log, vec![(0, C, 3)], "ゲストも選択を出る取消を入れた");
        assert_eq!(g.exits_done, 1);
    }

    // ================= 固定 seed の乱数 =================

    fn random_cfg(rng: &mut Rng, end: u32) -> Cfg {
        Cfg {
            end,
            host_phase: rng.below(16),
            host_period: 16 + rng.below(2),
            guest_phase: rng.below(16),
            guest_period: 16 + rng.below(2),
        }
    }

    /// 乱れる時間（ms）。これより後は落とさず 5〜14 ms で届け、送り直しが片付くのを見る。
    const CHAOS_UNTIL: u32 = 2500;
    const RANDOM_END: u32 = 4000;

    /// 20% 落とす・残りの 10% を重ねる・遅れは 3〜62 ms（入れ替わる）。
    fn chaos_faults(seed: u64) -> impl FnMut(Ctx) -> Vec<u32> {
        let mut r = Rng(seed ^ 0xA5A5_A5A5);
        move |c: Ctx| {
            if c.now >= CHAOS_UNTIL {
                return vec![5 + r.below(10)];
            }
            if r.chance(20) {
                vec![]
            } else if r.chance(10) {
                vec![3 + r.below(60), 3 + r.below(60)]
            } else {
                vec![3 + r.below(60)]
            }
        }
    }

    /// 両者がでたらめに押す（フレームごとに 10%）。ホストの value は 0〜3、ゲストは 4〜7
    /// （入った押下が誰のものか分かるように）。
    fn chaos_presses(seed: u64) -> impl FnMut(Role, u32, u16) -> Pr {
        let mut r = Rng(seed ^ 0x5A5A_5A5A);
        move |role: Role, now: u32, _: u16| {
            if now >= CHAOS_UNTIL || !r.chance(10) {
                return None;
            }
            let kind = if r.chance(50) { D } else { C };
            let base = if role == Role::Guest { 4 } else { 0 };
            Some((kind, base + r.below(4) as u8))
        }
    }

    #[derive(Default, Debug)]
    struct Tally {
        entries: usize,
        by_guest: usize,
        exits: u32,
        req_dropped: u32,
        step_dropped: u32,
        press_dropped: u32,
        words: usize,
        held_frames: u32,
    }

    fn random_new_run(
        seed: u64,
        exits: bool,
        busy_max: u32,
        tally: &mut Tally,
    ) -> Result<(), String> {
        let mut rng = Rng(seed);
        let cfg = random_cfg(&mut rng, RANDOM_END);
        let mut h = NewNode::new(Role::Host, rng.below(200), exits, seed ^ 1);
        let mut g = NewNode::new(Role::Guest, rng.below(200), exits, seed ^ 2);
        h.busy_max = busy_max;
        g.busy_max = busy_max;
        run(
            &mut h,
            &mut g,
            &cfg,
            &mut chaos_faults(seed),
            &mut chaos_presses(seed),
        );
        check_logs(&h.log, &g.log, true)?;
        for e in &h.log {
            if e.2 >= 4 && !g.reqs_sent.contains(e) {
                return Err(format!("ゲストが要求していない {e:?} が入った"));
            }
        }
        tally.words += words_ok(&h.sent_words, &g.got_words)?;
        tally.words += words_ok(&g.sent_words, &h.got_words)?;
        tally.entries += h.log.len();
        tally.by_guest += h.log.iter().filter(|e| e.2 >= 4).count();
        tally.exits += h.exits_done;
        tally.req_dropped += h.sel.counters().req_dropped;
        tally.step_dropped += g.sel.counters().step_dropped;
        tally.press_dropped += h.sel.counters().press_dropped + g.sel.counters().press_dropped;
        tally.held_frames += h.held_frames + g.held_frames;
        if h.applied_while_busy + g.applied_while_busy != 0 {
            return Err(format!(
                "演出の間に押下が入った: host {} guest {}",
                h.applied_while_busy, g.applied_while_busy
            ));
        }
        Ok(())
    }

    #[test]
    fn random_runs_keep_both_sides_in_step() {
        let mut tally = Tally::default();
        for seed in 0..1000u64 {
            if let Err(e) = random_new_run(seed, true, 0, &mut tally) {
                panic!("seed {seed}: {e}");
            }
        }
        eprintln!("selection random (1000 seed): {tally:?}");
        // 効く期間が本当に踏まれていること（0 なら何も測れていない）。
        assert!(tally.entries > 1000, "{tally:?}");
        assert!(tally.by_guest > 100, "{tally:?}");
        assert!(tally.exits > 100, "{tally:?}");
        assert!(tally.req_dropped > 100, "{tally:?}");
        assert!(tally.step_dropped > 100, "{tally:?}");
        assert!(tally.press_dropped > 100, "{tally:?}");
        assert!(tally.words > 10_000, "{tally:?}");
    }

    #[test]
    fn random_runs_hold_presses_through_transitions() {
        let mut tally = Tally::default();
        for seed in 0..1000u64 {
            if let Err(e) = random_new_run(seed, true, 6, &mut tally) {
                panic!("seed {seed}: {e}");
            }
        }
        eprintln!("selection random with transitions (1000 seed): {tally:?}");
        assert!(tally.entries > 1000, "{tally:?}");
        assert!(tally.by_guest > 100, "{tally:?}");
        assert!(tally.held_frames > 100, "{tally:?}");
        assert!(tally.words > 10_000, "{tally:?}");
    }

    #[test]
    fn random_runs_old_protocol_diverges_new_does_not() {
        let mut old_bad = 0u32;
        let mut new_entries = 0usize;
        for seed in 0..1000u64 {
            let mut rng = Rng(seed);
            let cfg = random_cfg(&mut rng, RANDOM_END);
            let mut oh = OldNode::new(true);
            let mut og = OldNode::new(false);
            run(
                &mut oh,
                &mut og,
                &cfg,
                &mut chaos_faults(seed),
                &mut chaos_presses(seed),
            );
            if check_logs(&oh.log, &og.log, true).is_err() {
                old_bad += 1;
            }
            let mut tally = Tally::default();
            if let Err(e) = random_new_run(seed, false, 0, &mut tally) {
                panic!("新しい手順が seed {seed} で食い違った: {e}");
            }
            new_entries += tally.entries;
        }
        eprintln!(
            "旧バージョンのコピーが食い違った seed: {old_bad} / 1000、新しい手順が入れた押下: {new_entries}"
        );
        assert!(old_bad > 0);
        assert!(new_entries > 1000);
    }

    // ================= 部品: ホストとゲストの規則 =================

    fn menu(step: u16, ack: u32, base: u32, ws: &[u16]) -> Menu {
        let mut words = [0u16; MAX_WORDS];
        words[..ws.len()].copy_from_slice(ws);
        Menu {
            step,
            ack,
            base,
            count: ws.len() as u8,
            words,
        }
    }

    /// 相手が来た状態のホスト / ゲスト。
    fn present(role: Role) -> Selection {
        let mut s = Selection::new(role);
        s.enter();
        s.on_menu(&menu(0, 0, 0, &[]));
        assert!(s.peer_present());
        s
    }

    #[test]
    fn guest_holds_a_step_until_ready() {
        let mut g = present(Role::Guest);
        let st = Step {
            step: 0,
            kind: D,
            value: 2,
            screen: 7,
        };
        g.on_step(&st);
        assert_eq!(g.next_apply(), Some(st));
        for _ in 0..3 {
            let fo = g.local_frame(0, None, false);
            assert_eq!(fo.apply, None);
            assert_eq!(g.step(), 0, "持ち越している間は step を進めない");
        }
        assert_eq!(g.next_apply(), Some(st));
        assert_eq!(g.local_frame(0, None, true).apply, Some(st));
        assert_eq!(g.step(), 1);
        assert_eq!(g.next_apply(), None);
    }

    #[test]
    fn host_holds_a_request_until_ready() {
        let mut h = present(Role::Host);
        let req = Step {
            step: 0,
            kind: C,
            value: 1,
            screen: 16,
        };
        h.on_step_req(&req);
        assert_eq!(h.next_apply(), Some(req));
        assert_eq!(h.local_frame(0, None, false).apply, None);
        assert_eq!(h.next_apply(), Some(req), "持ち越した");
        assert_eq!(h.local_frame(0, None, true).apply, Some(req));
        // 前の STEP がゲストに届く前は、自分の step の要求があっても next_apply に出ない。
        let mut h2 = present(Role::Host);
        h2.on_step_req(&req);
        assert!(h2.local_frame(0, None, true).apply.is_some());
        h2.held_req = Some(Press {
            kind: C,
            value: 1,
            screen: 16,
        });
        assert_eq!(h2.next_apply(), None, "前の STEP が届く前");
        // 持ち越している間に自分の押下が来たら、自分の押下を採る。
        h.on_step_req(&Step { step: 1, ..req });
        assert_eq!(h.next_apply(), Some(Step { step: 1, ..req }));
        assert_eq!(h.local_frame(0, None, false).apply, None);
        let own = Press {
            kind: D,
            value: 3,
            screen: 16,
        };
        assert_eq!(
            h.local_frame(0, Some(own), false).apply,
            Some(own.at(1)),
            "自分の押下はゲームが読む状態のときだけ渡すので、ready_ok に依らず入る"
        );
    }

    #[test]
    fn host_accepts_step_req_only_for_its_own_step() {
        let mut h = present(Role::Host);
        h.on_step_req(&Step {
            step: 0,
            kind: D,
            value: 4,
            screen: 0,
        });
        let fo = h.local_frame(0, None, true);
        assert_eq!(
            fo.apply,
            Some(Step {
                step: 0,
                kind: D,
                value: 4,
                screen: 0
            }),
            "同じ step の要求は入る"
        );
        h.on_menu(&menu(1, 0, 0, &[]));
        h.on_step_req(&Step {
            step: 0,
            kind: C,
            value: 5,
            screen: 0,
        });
        assert_eq!(h.local_frame(0, None, true).apply, None);
        assert_eq!(h.counters().req_dropped, 1);
    }

    #[test]
    fn guest_press_becomes_request_not_applied() {
        let mut g = present(Role::Guest);
        let fo = g.local_frame(0, pr(D, 4), true);
        assert_eq!(fo.apply, None);
        assert_eq!(g.pending_press(), pr(D, 4));
        assert_eq!(g.local_frame(0, pr(C, 5), true).apply, None);
        assert_eq!(g.pending_press(), pr(D, 4), "持っている間の押下は捨てる");
        assert_eq!(g.counters().press_dropped, 1);
        let req = Some(Message::StepReq(Step {
            step: 0,
            kind: D,
            value: 4,
            screen: 0,
        }));
        assert_eq!(g.poll_send(100).step, req);
        assert_eq!(g.poll_send(101).step, None);
        assert_eq!(g.poll_send(103).step, None);
        assert_eq!(g.poll_send(104).step, req);
        g.on_step(&Step {
            step: 0,
            kind: D,
            value: 1,
            screen: 0,
        });
        let fo = g.local_frame(0, None, true);
        assert_eq!(
            fo.apply,
            Some(Step {
                step: 0,
                kind: D,
                value: 1,
                screen: 0
            })
        );
        assert_eq!(g.pending_press(), None);
        assert_eq!(g.poll_send(200).step, None);
    }

    #[test]
    fn guest_applies_steps_in_order_once() {
        let mut g = present(Role::Guest);
        let s0 = Step {
            step: 0,
            kind: D,
            value: 1,
            screen: 0,
        };
        g.on_step(&Step {
            step: 1,
            kind: D,
            value: 9,
            screen: 0,
        });
        assert_eq!(
            g.local_frame(0, None, true).apply,
            None,
            "新しすぎる STEP は入れない"
        );
        g.on_step(&s0);
        g.on_step(&s0);
        assert_eq!(g.local_frame(0, None, true).apply, Some(s0));
        g.on_step(&s0);
        assert_eq!(g.local_frame(0, None, true).apply, None, "重複は入れない");
        assert_eq!(g.step(), 1);
    }

    #[test]
    fn host_step_resend_until_guest_moves_past() {
        let mut h = present(Role::Host);
        let st = h.local_frame(0, pr(D, 2), true).apply;
        assert!(st.is_some());
        let m = st.map(Message::Step);
        assert_eq!(h.poll_send(10).step, m);
        assert_eq!(h.poll_send(13).step, None);
        assert_eq!(h.poll_send(14).step, m);
        h.on_menu(&menu(0, 0, 0, &[]));
        assert_eq!(h.poll_send(18).step, m, "step 0 の MENU は ack にならない");
        h.on_menu(&menu(1, 0, 0, &[]));
        assert_eq!(h.poll_send(22).step, None);
        assert_eq!(h.poll_send(40).step, None);
    }

    // ================= 部品: MENU の入力ワード =================

    #[test]
    fn menu_words_in_order_one_per_frame_then_hold_last() {
        let mut g = present(Role::Guest);
        g.on_menu(&menu(0, 0, 0, &[0x10, 0x20, 0x30]));
        let got: Vec<u16> = (0..5)
            .map(|_| g.local_frame(0, None, true).remote)
            .collect();
        assert_eq!(got, vec![0x10, 0x20, 0x30, 0x30, 0x30]);
        assert_eq!(g.build_menu().map(|m| m.ack), Some(3));
    }

    #[test]
    fn menu_step_change_keeps_the_last_word_only_on_the_same_screen() {
        let mut h = present(Role::Host);
        h.on_menu(&menu(0, 0, 0, &[0x40]));
        let fo = h.local_frame(0x77, scr_press(D, 2, 7), true);
        assert_eq!(fo.remote, 0x40, "押下のフレームの語は移る前の画面で使う");
        assert!(fo.apply.is_some());
        let m = h.build_menu().expect("選択の中");
        assert_eq!(
            (m.step, m.ack, m.base, m.count),
            (1, 0, 0, 0),
            "語の番号は step ごとに 0 から。押下のフレームの語は積まない"
        );
        let dropped = h.counters().menu_dropped;
        h.on_menu(&menu(0, 0, 1, &[0x41]));
        assert_eq!(h.counters().menu_dropped, dropped + 1);
        assert_eq!(h.local_frame(0, None, true).remote, 0);
        assert_eq!(h.local_frame(0, None, true).remote, 0);

        let mut h = present(Role::Host);
        h.on_menu(&menu(0, 0, 0, &[0x40]));
        let fo = h.local_frame(0x77, scr_press(D, chara_value(0, false, 3), 8), true);
        assert_eq!(fo.remote, 0x40);
        assert!(fo.apply.is_some());
        let m = h.build_menu().expect("選択の中");
        assert_eq!((m.step, m.ack, m.base, m.count), (1, 0, 0, 1));
        assert_eq!(
            m.words[0], 0x77,
            "押下のフレームの自分の語は新しい step の 0 番"
        );
        assert_eq!(
            h.local_frame(0, None, true).remote,
            0x40,
            "直前の語を続ける"
        );
        let dropped = h.counters().menu_dropped;
        h.on_menu(&menu(0, 0, 1, &[0x41]));
        assert_eq!(
            h.counters().menu_dropped,
            dropped,
            "前の step の残りは受ける"
        );
        assert_eq!(h.local_frame(0, None, true).remote, 0x41);
        assert_eq!(
            h.build_menu().map(|m| m.ack),
            Some(0),
            "前の step の語は今の step の ack に数えない"
        );
    }

    #[test]
    fn menu_gap_jumps_forward_and_old_menu_does_not_rewind() {
        let mut g = present(Role::Guest);
        g.on_menu(&menu(0, 0, 0, &[1, 2, 3]));
        assert_eq!(g.local_frame(0, None, true).remote, 1);
        g.on_menu(&menu(0, 0, 10, &[11, 12]));
        assert_eq!(g.local_frame(0, None, true).remote, 11);
        g.on_menu(&menu(0, 0, 0, &[1, 2, 3]));
        assert_eq!(g.local_frame(0, None, true).remote, 12);
        assert_eq!(g.local_frame(0, None, true).remote, 12);
        assert_eq!(g.build_menu().map(|m| m.ack), Some(12));
    }

    #[test]
    fn menu_other_step_discarded() {
        let mut g = Selection::new(Role::Guest);
        g.enter();
        g.on_menu(&menu(1, 0, 0, &[0x55]));
        assert!(!g.peer_present());
        assert_eq!(g.local_frame(0, None, true).remote, 0);
        assert_eq!(g.counters().menu_dropped, 1);
        g.on_menu(&menu(0, 0, 0, &[0x55]));
        assert!(g.peer_present());
        assert_eq!(g.local_frame(0, None, true).remote, 0x55);
    }

    #[test]
    fn menu_sender_sends_newest_32_unacked() {
        let mut h = present(Role::Host);
        for i in 0..40u16 {
            h.local_frame(100 + i, None, true);
        }
        let m = h.build_menu().expect("選択の中");
        assert_eq!((m.base, m.count), (8, 32));
        assert_eq!(m.words[0], 108);
        assert_eq!(m.words[31], 139);
        h.on_menu(&menu(0, 20, 0, &[]));
        let m = h.build_menu().expect("選択の中");
        assert_eq!((m.base, m.count), (20, 20));
        assert_eq!(m.words[0], 120);
        h.on_menu(&menu(0, 999, 0, &[]));
        let m = h.build_menu().expect("選択の中");
        assert_eq!((m.base, m.count), (40, 0));
        assert_eq!(m.step, 0);
    }

    #[test]
    fn build_menu_only_inside_selection() {
        let mut h = Selection::new(Role::Host);
        assert!(h.build_menu().is_none(), "入る前");
        h.enter();
        h.on_menu(&menu(0, 0, 0, &[]));
        let st = h.local_frame(0, pr(C, 3), true).apply;
        assert!(st.is_some(), "選択を出る取消を入れた");
        h.leave();
        assert!(h.build_menu().is_none(), "出た後");
        // ゲストが出た後に作った MENU が、次の選択に入ったホストに遅れて届くと「来た」になる。だから作らない。
        let mut tail = Selection::new(Role::Guest);
        tail.enter();
        tail.on_menu(&menu(0, 0, 0, &[]));
        tail.on_step(&Step {
            step: 0,
            kind: C,
            value: 3,
            screen: 0,
        });
        assert!(tail.local_frame(0, None, true).apply.is_some());
        let late = tail.build_menu().expect("leave を忘れると作れてしまう");
        h.enter();
        assert!(!h.peer_present());
        h.on_menu(&late);
        assert!(
            h.peer_present(),
            "出た後の MENU で「来た」になる（害が本当にある）"
        );
    }

    #[test]
    fn menu_backlog_over_32_drops_oldest() {
        let mut g = present(Role::Guest);
        let a: Vec<u16> = (0..32).collect();
        let b: Vec<u16> = (32..41).collect();
        g.on_menu(&menu(0, 0, 0, &a));
        g.on_menu(&menu(0, 0, 32, &b));
        let got: Vec<u16> = (0..33)
            .map(|_| g.local_frame(0, None, true).remote)
            .collect();
        let want: Vec<u16> = (9..41).chain([40]).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn enter_resets_presence_and_remote() {
        let mut g = present(Role::Guest);
        g.on_menu(&menu(0, 0, 0, &[7, 8, 9]));
        assert_eq!(g.local_frame(0, None, true).remote, 7);
        g.leave();
        g.enter();
        assert!(!g.peer_present());
        assert_eq!(g.local_frame(0, None, true).remote, 0);
        g.on_menu(&menu(0, 0, 3, &[10]));
        assert!(g.peer_present());
        assert_eq!(g.local_frame(0, None, true).remote, 10);
    }

    // ================= 部品: SELECT =================

    fn sel(map: u8, difficulty: u8) -> Select {
        Select {
            match_no: 0x7777,
            chara: 3,
            color: 1,
            life: 5,
            charge: 2,
            map,
            difficulty,
            cpu_1p: 9,
            cpu_2p: 8,
            background: 7,
            rng_seed: 0x4242,
            option_digest: 0xDEAD_BEEF,
        }
    }

    #[test]
    fn select_resent_until_peer_input() {
        let mut h = Selection::new(Role::Host);
        h.enter();
        h.on_peer_input();
        h.set_local_select(&sel(1, 2));
        let first = h.poll_send(50).select;
        assert!(matches!(first, Some(Message::Select(s)) if s.match_no == 0 && s.map == 1));
        assert_eq!(h.poll_send(53).select, None);
        assert_eq!(h.poll_send(54).select, first);
        h.on_peer_input();
        assert_eq!(h.poll_send(58).select, None);
        assert_eq!(h.poll_send(100).select, None);
    }

    #[test]
    fn select_other_match_no_and_duplicate_ignored() {
        let mut g = Selection::new(Role::Guest);
        g.enter();
        let mut other = sel(1, 1);
        other.match_no = 1;
        g.on_select(&other);
        assert!(g.peer_select.is_none());
        let mut ok = sel(2, 2);
        ok.match_no = 0;
        g.on_select(&ok);
        let mut dup = sel(3, 3);
        dup.match_no = 0;
        g.on_select(&dup);
        assert_eq!(g.peer_select.map(|s| s.map), Some(2));
        assert_eq!(g.counters().select_dropped, 2);
    }

    #[test]
    fn select_mismatch_takes_host_values() {
        let mut h = Selection::new(Role::Host);
        let mut g = Selection::new(Role::Guest);
        h.enter();
        g.enter();
        h.set_local_select(&sel(3, 1));
        let mut gs = sel(5, 1);
        gs.option_digest = 1;
        g.set_local_select(&gs);
        assert!(h.agreed().is_none() && !h.selects_ready());
        let Some(Message::Select(hs)) = h.poll_send(0).select else {
            panic!("ホストの SELECT が出ない")
        };
        let Some(Message::Select(gsent)) = g.poll_send(0).select else {
            panic!("ゲストの SELECT が出ない")
        };
        assert_eq!(
            (gsent.cpu_1p, gsent.cpu_2p, gsent.background, gsent.rng_seed),
            (0, 0, 0, 0),
            "ゲストはホストだけの欄を 0 で送る"
        );
        h.on_select(&gsent);
        g.on_select(&hs);
        assert!(h.selects_ready() && g.selects_ready());
        for a in [h.agreed(), g.agreed()] {
            let a = a.expect("揃った");
            assert_eq!((a.map, a.difficulty), (3, 1));
            assert_eq!(
                (a.cpu_1p, a.cpu_2p, a.background, a.rng_seed),
                (9, 8, 7, 0x4242)
            );
            assert_eq!(
                a.mismatch,
                Mismatch {
                    map: true,
                    difficulty: false,
                    option_digest: true
                }
            );
        }
        assert_eq!(h.agreed().map(|a| a.peer), Some(gsent));
        assert_eq!(g.agreed().map(|a| a.peer), Some(hs));
    }

    #[test]
    fn host_final_step_resend_stops_on_guest_select() {
        let mut h = present(Role::Host);
        let st = h.local_frame(0, pr(D, 4), true).apply;
        assert!(st.is_some());
        let m = st.map(Message::Step);
        h.leave();
        h.set_local_select(&sel(4, 0));
        assert_eq!(h.poll_send(0).step, m);
        let mut stale = sel(4, 0);
        stale.match_no = 9;
        h.on_select(&stale);
        assert_eq!(
            h.poll_send(4).step,
            m,
            "別の試合の SELECT は ack にならない"
        );
        let mut gs = sel(4, 0);
        gs.match_no = 0;
        h.on_select(&gs);
        assert_eq!(h.poll_send(8).step, None);
    }

    #[test]
    fn match_no_advances_after_a_match() {
        let mut g = Selection::new(Role::Guest);
        g.enter();
        g.leave();
        g.enter();
        assert_eq!(g.match_no(), 0, "SELECT を置かずに出た選択では進まない");
        g.set_local_select(&sel(1, 1));
        g.leave();
        g.enter();
        assert_eq!(g.match_no(), 1);
        assert!(
            g.poll_send(0).select.is_none(),
            "前の試合の SELECT は送らない"
        );
    }

    // ================= 合わせ =================

    const AL: StepKind = StepKind::Align;
    const SCR: u8 = 8;

    fn align_press(value: u8, screen: u8) -> Option<Press> {
        Some(Press {
            kind: AL,
            value,
            screen,
        })
    }

    #[test]
    fn align_is_due_only_for_the_host_once_per_screen() {
        let mut h = Selection::new(Role::Host);
        assert!(!h.align_due(SCR), "選択の外");
        h.enter();
        assert!(!h.align_due(SCR), "相手が来る前");
        h.on_menu(&menu(0, 0, 0, &[]));
        assert!(h.align_due(SCR));
        assert!(!h.is_aligned(SCR));
        let fo = h.local_frame(0, align_press(0x21, SCR), true);
        assert_eq!(
            fo.apply.map(|s| (s.step, s.kind, s.value)),
            Some((0, AL, 0x21))
        );
        assert!(h.is_aligned(SCR));
        assert!(!h.align_due(SCR), "この画面の合わせは済み");
        assert!(!h.is_aligned(7), "別の画面は済みではない");
        assert!(!h.align_due(7), "前の STEP が届く前");
        h.on_menu(&menu(1, 0, 0, &[]));
        assert!(h.align_due(7));
        h.leave();
        assert!(!h.align_due(7), "選択の外");
        h.enter();
        assert!(!h.is_aligned(SCR), "入り直したら合わせはやり直し");
        let g = present(Role::Guest);
        assert!(!g.align_due(SCR), "ゲストは出さない");
    }

    #[test]
    fn guest_takes_an_align_step_and_host_refuses_it_in_a_request() {
        let mut g = present(Role::Guest);
        let st = Step {
            step: 0,
            kind: AL,
            value: 0x21,
            screen: SCR,
        };
        g.on_step(&st);
        assert_eq!(g.next_apply(), Some(st));
        assert!(!g.is_aligned(SCR));
        assert_eq!(g.local_frame(0, None, true).apply, Some(st));
        assert_eq!(
            (g.step(), g.is_aligned(SCR), g.is_aligned(7)),
            (1, true, false)
        );
        let mut h = present(Role::Host);
        h.on_step_req(&st);
        assert_eq!(h.counters().impossible, 1);
        assert_eq!(h.next_apply(), None, "要求として持たない");
    }

    #[test]
    fn first_menu_after_enter_starts_from_the_newest_word() {
        let mut g = Selection::new(Role::Guest);
        g.enter();
        let old: Vec<u16> = (100..132).collect();
        g.on_menu(&menu(0, 0, 0, &old));
        assert_eq!(g.local_frame(0, None, true).remote, 131, "最新の語から");
        g.on_menu(&menu(0, 0, 32, &[132, 133, 134]));
        let got: Vec<u16> = (0..3)
            .map(|_| g.local_frame(0, None, true).remote)
            .collect();
        assert_eq!(got, vec![132, 133, 134]);
        let st = Step {
            step: 0,
            kind: AL,
            value: 0,
            screen: SCR,
        };
        g.on_step(&st);
        assert!(g.local_frame(0, None, true).apply.is_some());
        g.on_menu(&menu(1, 0, 0, &[7, 8, 9]));
        let got: Vec<u16> = (0..3)
            .map(|_| g.local_frame(0, None, true).remote)
            .collect();
        assert_eq!(got, vec![7, 8, 9], "step の途中から始まる形ではない");
        g.leave();
        g.enter();
        g.on_menu(&menu(1, 0, 3, &[1, 2, 3, 4]));
        assert_eq!(g.local_frame(0, None, true).remote, 4);
    }

    #[test]
    fn idle_words_are_skipped_only_while_backlogged() {
        let (r, z) = (0x80u16, 0u16);
        let mut g = present(Role::Guest);
        g.on_menu(&menu(0, 0, 0, &[z, z, z, z, z, z, z, z, r, z, z, r]));
        let got: Vec<u16> = (0..5)
            .map(|_| g.local_frame(0, None, true).remote)
            .collect();
        assert_eq!(
            got,
            vec![r, z, r, r, r],
            "先頭の 8 個と、間の 2 個のうち 1 個を飛ばす"
        );
        let mut g = present(Role::Guest);
        g.on_menu(&menu(0, 0, 0, &[r, r, r, z, r]));
        let got: Vec<u16> = (0..5)
            .map(|_| g.local_frame(0, None, true).remote)
            .collect();
        assert_eq!(got, vec![r, r, r, z, r]);
        let mut g = present(Role::Guest);
        g.on_menu(&menu(0, 0, 0, &[z]));
        assert_eq!(g.local_frame(0, None, true).remote, z);
        assert_eq!(g.build_menu().map(|m| m.ack), Some(1));
    }

    // ---- 合わせの 2 台の模型（カーソルを持つ） ----

    use crate::input_model::keys;
    use crate::menu_model::{APPLY_CLEAR_BITS, align_chara_parts, align_chara_value};

    /// 合わせの検査の 1 台。画面 `SCR`（キャラ選択。2 席分のカーソル 0〜15）を持ち、dll の `menu_tick` と同じ順に `Selection` を呼ぶ。
    /// ゲームは語の押した瞬間（右 = +1、左 = −1）で席ごとのカーソルを動かす。合わせを受けたゲストは、書いてからその tick は動かさない。
    struct CursorNode {
        sel: Selection,
        role: Role,
        my: usize,
        cur: [u8; 2],
        prev: [u16; 2],
        enter_at: u32,
        moves: Vec<(u32, u16)>,
        frame_no: u32,
        changes: Vec<(u32, [u8; 2])>,
        aligned_at: Option<u32>,
        /// 合わせが届く前に、ゲームへ入れた入力ワードの方向・Ctrl・Shift の数（ゲストは 0 であること）。
        pre_align_dirs: u32,
    }

    impl CursorNode {
        fn new(role: Role, cur: [u8; 2], enter_at: u32, moves: Vec<(u32, u16)>) -> Self {
            Self {
                sel: Selection::new(role),
                role,
                my: usize::from(role == Role::Guest),
                cur,
                prev: [0; 2],
                enter_at,
                moves,
                frame_no: 0,
                changes: Vec::new(),
                aligned_at: None,
                pre_align_dirs: 0,
            }
        }
    }

    impl Node for CursorNode {
        type Msg = Message;

        fn frame(&mut self, now: u32, _press: Pr, out: &mut Vec<Message>) {
            if !self.sel.is_active() {
                if now < self.enter_at || self.frame_no > 0 {
                    return;
                }
                self.sel.enter();
            }
            self.frame_no += 1;
            let human = self
                .moves
                .iter()
                .find(|m| m.0 == self.frame_no)
                .map_or(0, |m| m.1);
            let aligned = self.sel.is_aligned(SCR);
            let mute = self.role == Role::Guest && !aligned;
            let local = if mute { 0 } else { human };
            let press = (self.role == Role::Host
                && self.sel.align_due(SCR)
                && local & APPLY_CLEAR_BITS == 0)
                .then_some(Press {
                    kind: AL,
                    value: align_chara_value([u32::from(self.cur[0]), u32::from(self.cur[1])]),
                    screen: SCR,
                });
            let fo = self.sel.local_frame(local, press, true);
            let remote = if mute { 0 } else { fo.remote };
            let mut words = [0u16; 2];
            words[self.my] = local;
            words[1 - self.my] = remote;
            if let Some(st) = fo.apply {
                assert_eq!(st.kind, AL, "この模型は合わせしか入れない");
                self.aligned_at = self.aligned_at.or(Some(now));
                if self.role == Role::Guest {
                    let [a, b] = align_chara_parts(st.value);
                    self.cur = [a as u8, b as u8];
                    words = [0; 2];
                }
            } else if !aligned && words.iter().any(|w| w & APPLY_CLEAR_BITS != 0) {
                self.pre_align_dirs += 1;
            }
            let before = self.cur;
            for (s, w) in words.iter().enumerate() {
                let edge = w & !self.prev[s];
                if edge & keys::RIGHT != 0 {
                    self.cur[s] = (self.cur[s] + 1) % 16;
                }
                if edge & keys::LEFT != 0 {
                    self.cur[s] = (self.cur[s] + 15) % 16;
                }
                self.prev[s] = *w;
            }
            if self.cur != before || fo.apply.is_some() && self.role == Role::Guest {
                self.changes.push((now, self.cur));
            }
            if let Some(m) = self.sel.build_menu() {
                out.push(Message::Menu(m));
            }
        }

        fn recv(&mut self, _now: u32, m: Message, _out: &mut Vec<Message>) {
            match m {
                Message::Menu(x) => self.sel.on_menu(&x),
                Message::StepReq(x) => self.sel.on_step_req(&x),
                Message::Step(x) => self.sel.on_step(&x),
                _ => {}
            }
        }

        fn tick(&mut self, now: u32, out: &mut Vec<Message>) {
            let o = self.sel.poll_send(now);
            out.extend([o.step, o.select].into_iter().flatten());
        }

        fn cur_step(&self) -> u16 {
            self.sel.step()
        }

        fn label(m: &Message) -> (Kind, u16) {
            NewNode::label(m)
        }
    }

    fn no_presses(_: Role, _: u32, _: u16) -> Pr {
        None
    }

    fn fixed(delay: u32) -> impl FnMut(Ctx) -> Vec<u32> {
        move |_| vec![delay]
    }

    #[test]
    fn cursors_meet_at_the_hosts_values_when_both_enter() {
        for delay in [1u32, 16, 40] {
            let mut h = CursorNode::new(Role::Host, [3, 8], 0, vec![]);
            let mut g = CursorNode::new(Role::Guest, [5, 10], 0, vec![]);
            let cfg = Cfg { end: 600, ..CFG };
            run(&mut h, &mut g, &cfg, &mut fixed(delay), &mut no_presses);
            assert_eq!(h.cur, [3, 8], "ホストは自分の値のまま（片道 {delay}）");
            assert_eq!(g.cur, [3, 8], "ゲストがホストの値へ寄る（片道 {delay}）");
            assert!(g.aligned_at.is_some() && h.aligned_at.is_some());
            assert_eq!(g.pre_align_dirs, 0);
        }
    }

    #[test]
    fn host_moves_before_the_guest_arrives_and_both_move_after() {
        for delay in [1u32, 16, 40] {
            let host_moves = vec![
                (3, keys::RIGHT),
                (8, keys::RIGHT),
                (12, keys::RIGHT),
                (18, keys::LEFT),
                (90, keys::LEFT),
                (100, keys::LEFT),
            ];
            let guest_moves = vec![(60, keys::RIGHT), (70, keys::RIGHT)];
            let mut h = CursorNode::new(Role::Host, [3, 8], 0, host_moves);
            let mut g = CursorNode::new(Role::Guest, [5, 10], 500, guest_moves);
            let cfg = Cfg { end: 3000, ..CFG };
            run(&mut h, &mut g, &cfg, &mut fixed(delay), &mut no_presses);
            assert_eq!(h.cur, [3, 10], "ホスト（片道 {delay}）");
            assert_eq!(g.cur, h.cur, "ゲスト（片道 {delay}）");
        }
    }

    #[test]
    fn a_guest_that_enters_first_and_moves_meets_the_host_when_it_arrives() {
        for delay in [1u32, 16, 40] {
            let guest_moves = vec![(3, keys::LEFT), (8, keys::LEFT), (12, keys::LEFT)];
            let mut h = CursorNode::new(Role::Host, [3, 8], 500, vec![]);
            let mut g = CursorNode::new(Role::Guest, [5, 10], 0, guest_moves);
            let cfg = Cfg { end: 1500, ..CFG };
            run(&mut h, &mut g, &cfg, &mut fixed(delay), &mut no_presses);
            assert_eq!(g.pre_align_dirs, 0, "ゲストは合わせの前に方向を入れない");
            assert_eq!(
                g.changes.first().map(|c| c.1),
                Some([3, 8]),
                "ゲストのカーソルは、最初の変化が合わせの値（自分の動きでは変わらない。片道 {delay}）"
            );
            assert_eq!((h.cur, g.cur), ([3, 8], [3, 8]), "片道 {delay}");
        }
    }

    #[test]
    fn a_late_guest_sees_the_hosts_move_within_a_few_frames() {
        for delay in [1u32, 16, 40] {
            // ホストは右・左を 30 回ずつ（溜まる語を 32 より多くする）。
            let mut host_moves: Vec<(u32, u16)> = (0..60)
                .map(|i| (3 + 3 * i, if i % 2 == 0 { keys::RIGHT } else { keys::LEFT }))
                .collect();
            host_moves.push((120, keys::RIGHT));
            let mut h = CursorNode::new(Role::Host, [3, 8], 0, host_moves);
            let mut g = CursorNode::new(Role::Guest, [5, 10], 1000, vec![]);
            let cfg = Cfg { end: 3000, ..CFG };
            run(&mut h, &mut g, &cfg, &mut fixed(delay), &mut no_presses);
            assert_eq!(g.cur, h.cur, "揃う（片道 {delay}）");
            let host_last = h.changes.last().expect("ホストが動いた").0;
            let guest_last = g.changes.last().expect("ゲストが動いた").0;
            assert!(guest_last >= host_last);
            let bound = delay + 16 + 8 + 16;
            assert!(
                guest_last - host_last <= bound,
                "ゲストの見え方が遅れている: {} ms（片道 {delay}、上限 {bound}）",
                guest_last - host_last
            );
        }
    }

    #[test]
    fn random_runs_end_with_equal_cursors() {
        // 効く期間が本当に踏まれたか（0 なら何も測れていない）。
        let (mut host_before, mut guest_after, mut host_after) = (0u32, 0u32, 0u32);
        for seed in 0..300u64 {
            let mut rng = Rng(seed ^ 0xC0FFEE);
            let cfg = random_cfg(&mut rng, RANDOM_END);
            let mk = |rng: &mut Rng, n: u32| -> Vec<(u32, u16)> {
                let mut frames: Vec<u32> = (0..n).map(|_| 1 + rng.below(120)).collect();
                frames.sort_unstable();
                frames.dedup();
                frames
                    .into_iter()
                    .map(|f| {
                        let w = match rng.below(3) {
                            0 => keys::RIGHT,
                            1 => keys::LEFT,
                            _ => keys::SHIFT,
                        };
                        (f, w)
                    })
                    .collect()
            };
            let hm = mk(&mut rng, 25);
            let gm = mk(&mut rng, 25);
            let hc = [rng.below(16) as u8, rng.below(16) as u8];
            let gc = [rng.below(16) as u8, rng.below(16) as u8];
            let mut h = CursorNode::new(Role::Host, hc, rng.below(400), hm);
            let mut g = CursorNode::new(Role::Guest, gc, rng.below(400), gm);
            run(
                &mut h,
                &mut g,
                &cfg,
                &mut chaos_faults(seed),
                &mut no_presses,
            );
            assert_eq!(g.pre_align_dirs, 0, "seed {seed}");
            assert!(
                h.aligned_at.is_some() && g.aligned_at.is_some(),
                "seed {seed}: 合わせが入らない"
            );
            assert_eq!(h.cur, g.cur, "seed {seed}: 最後にカーソルが分かれた");
            let t = g.aligned_at.unwrap_or(0);
            host_before += u32::from(h.changes.iter().any(|c| c.0 < t));
            guest_after += u32::from(g.changes.iter().skip(1).any(|c| c.0 > t));
            host_after += u32::from(h.changes.iter().any(|c| c.0 > t));
        }
        eprintln!(
            "合わせの乱数の実行 (300 seed): 合わせの前にホストが動いた {host_before}、合わせの後にゲストが動いた {guest_after}、合わせの後にホストが動いた {host_after}"
        );
        assert!(host_before > 30 && guest_after > 100 && host_after > 100);
    }

    // ================= 同じ画面に残る step をまたぐ相手の入力ワード =================

    use crate::menu_model::{SCREEN_CHARA, SCREEN_DIFFICULTY, SCREEN_MAP, chara_value};

    /// ゲストに STEP の列 `steps`（kind, value, screen）を入れる。各 STEP の前にその step の相手の入力ワード 0x10 を 1 つ届け、最後の STEP の後に
    /// 前の step の入力ワード 0x11 を遅れて届ける。返すのは、最後の STEP の後の 2 フレームの相手の入力ワードと、遅れた入力ワードで `menu_dropped` が増えたか。
    fn after_last_step(steps: &[(StepKind, u8, u8)]) -> ([u16; 2], bool) {
        let mut g = present(Role::Guest);
        let mut last = 0u16;
        for (i, &(kind, value, screen)) in steps.iter().enumerate() {
            let step = i as u16;
            g.on_menu(&menu(step, 0, 0, &[0x10]));
            g.on_step(&Step {
                step,
                kind,
                value,
                screen,
            });
            let fo = g.local_frame(0, None, true);
            assert!(fo.apply.is_some(), "{steps:?} の {i} 番目が入らない");
            last = step;
        }
        let dropped = g.counters().menu_dropped;
        g.on_menu(&menu(last, 0, 1, &[0x11]));
        let a = g.local_frame(0, None, true).remote;
        let b = g.local_frame(0, None, true).remote;
        ([a, b], g.counters().menu_dropped > dropped)
    }

    #[test]
    fn only_steps_that_stay_on_the_screen_carry_the_old_words() {
        let cv = |seat| chara_value(seat, false, 5);
        let (c8, c7, c16) = (SCREEN_CHARA, SCREEN_DIFFICULTY, SCREEN_MAP);
        let stays: &[&[(StepKind, u8, u8)]] = &[
            &[(D, cv(0), c8)],
            &[(D, cv(1), c8)],
            &[(D, cv(1), c8), (C, cv(1), c8)],
            &[(D, 2, c7), (D, cv(0), c8)],
            &[(C, 0, c16), (D, cv(1), c8)],
        ];
        for steps in stays {
            assert_eq!(
                after_last_step(steps),
                ([0x11, 0x11], false),
                "同じ画面に残る: {steps:?}"
            );
        }
        let moves: &[&[(StepKind, u8, u8)]] = &[
            &[(D, cv(0), c8), (D, cv(1), c8)],
            &[(C, cv(0), c8)],
            &[(D, 2, c7)],
            &[(C, 0, c16)],
            &[(StepKind::Align, 0x21, c8)],
            &[(D, cv(0), 0)],
            &[(D, 2, c7), (D, cv(0), c16)],
            &[(D, 2, c7), (D, cv(0), c16), (D, cv(1), c8)],
        ];
        for steps in moves {
            assert_eq!(
                after_last_step(steps),
                ([0, 0], true),
                "画面が移る: {steps:?}"
            );
        }
    }

    #[test]
    fn carried_words_are_passed_once_and_in_order() {
        let (a, b, c, d, e, f, x) = (0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x99);
        let mut g = present(Role::Guest);
        g.on_menu(&menu(0, 0, 0, &[a, b]));
        g.on_step(&Step {
            step: 0,
            kind: D,
            value: chara_value(1, false, 2),
            screen: SCREEN_CHARA,
        });
        let fo = g.local_frame(0, None, true);
        assert_eq!((fo.remote, fo.apply.is_some()), (a, true));
        let mut got = Vec::new();
        g.on_menu(&menu(0, 0, 0, &[a, b, c])); // 重複と新しい語
        g.on_menu(&menu(0, 0, 2, &[c])); // 重複だけ
        got.push(g.local_frame(0, None, true).remote);
        got.push(g.local_frame(0, None, true).remote);
        g.on_menu(&menu(0, 0, 1, &[b, c, d])); // 重なり
        g.on_menu(&menu(1, 0, 0, &[e, f])); // 新しい step
        got.push(g.local_frame(0, None, true).remote);
        got.push(g.local_frame(0, None, true).remote);
        let dropped = g.counters().menu_dropped;
        g.on_menu(&menu(0, 0, 0, &[a, b, c, d, x])); // 新しい語を渡し始めた後の前の step の語
        assert_eq!(g.counters().menu_dropped, dropped + 1);
        got.push(g.local_frame(0, None, true).remote);
        got.push(g.local_frame(0, None, true).remote);
        assert_eq!(got, vec![b, c, d, e, f, f]);
        assert_eq!(g.build_menu().map(|m| m.ack), Some(2));
    }

    type WordRec = (u32, u16, u32, u16);

    /// 同じ画面に残る step の検査の 1 台。キャラ選択（画面 8）に入り、毎フレーム台本の語を積み、押下は自分の画面の模型から作る。
    /// 期待は、受け手が渡す相手の入力ワードが、送り手が自分のゲームへ入れた入力ワードの列と同じ並びで、画面の区切りをまたがないこと。
    struct ScreenNode {
        sel: Selection,
        seat: usize,
        model: Model,
        seg: u32,
        frame_no: u32,
        word: Box<dyn Fn(u32) -> u16>,
        auto: bool,
        /// 自分の入力ワード。step は相手がその入力ワードを受け取る step（同じ画面に残る押下を入れたフレームの入力ワードは次の step）。
        sent: Vec<WordRec>,
        /// 相手の入力ワード。step はゲームへ渡したフレームの（押下を入れる前の）step。
        got: Vec<WordRec>,
        log: Vec<Entry>,
        applied_at: Vec<u32>,
        stays: u32,
        moves: u32,
    }

    impl ScreenNode {
        fn new(role: Role, word: Box<dyn Fn(u32) -> u16>, auto: bool) -> Self {
            Self {
                sel: Selection::new(role),
                seat: usize::from(role == Role::Guest),
                model: Model::new(SCREEN_CHARA),
                seg: 0,
                frame_no: 0,
                word,
                auto,
                sent: Vec::new(),
                got: Vec::new(),
                log: Vec::new(),
                applied_at: Vec::new(),
                stays: 0,
                moves: 0,
            }
        }

        /// でたらめの実行の押下: キャラ選択では自分の席を決める / 決めていれば取り消す、マップでは取消。
        /// 選択を出る押下と、決めていない席の取消（難易度へ戻る）は出さない。
        fn auto_kind(&self) -> Option<StepKind> {
            match self.model.screen() {
                SCREEN_CHARA if self.model.decided(self.seat).is_some() => Some(C),
                SCREEN_CHARA => Some(D),
                SCREEN_MAP => Some(C),
                _ => None,
            }
        }
    }

    impl Node for ScreenNode {
        type Msg = Message;

        fn frame(&mut self, now: u32, press: Pr, out: &mut Vec<Message>) {
            if !self.sel.is_active() {
                if self.frame_no > 0 {
                    return;
                }
                self.sel.enter();
            }
            self.frame_no += 1;
            let w = (self.word)(self.frame_no);
            let screen = self.model.screen();
            let kind = match press {
                Some(_) if self.auto => self.auto_kind(),
                Some((k, _)) => Some(k),
                None => None,
            };
            let press = kind.map(|kind| Press {
                kind,
                value: if screen == SCREEN_CHARA {
                    chara_value(self.seat, false, (self.frame_no % 16) as u8)
                } else {
                    0
                },
                screen,
            });
            let s = self.sel.step();
            let seg = self.seg;
            let fo = self.sel.local_frame(w, press, true);
            self.got.push((now, s, seg, fo.remote));
            let mut word_step = s;
            if let Some(st) = fo.apply {
                let outcome = self
                    .model
                    .apply(&st)
                    .expect("両者は同じ順に STEP を入れるので、模型と食い違わない");
                if outcome == Outcome::Stay
                    && st.kind != StepKind::Align
                    && self.model.screen() == screen
                {
                    word_step = s + 1;
                    self.stays += 1;
                } else {
                    self.seg += 1;
                    self.moves += 1;
                }
                self.log.push((st.step, st.kind, st.value));
                self.applied_at.push(now);
            }
            self.sent.push((now, word_step, seg, w));
            if let Some(m) = self.sel.build_menu() {
                out.push(Message::Menu(m));
            }
        }

        fn recv(&mut self, _now: u32, m: Message, _out: &mut Vec<Message>) {
            match m {
                Message::Menu(x) => self.sel.on_menu(&x),
                Message::StepReq(x) => self.sel.on_step_req(&x),
                Message::Step(x) => self.sel.on_step(&x),
                _ => {}
            }
        }

        fn tick(&mut self, now: u32, out: &mut Vec<Message>) {
            let o = self.sel.poll_send(now);
            out.extend([o.step, o.select].into_iter().flatten());
        }

        fn cur_step(&self) -> u16 {
            self.sel.step()
        }

        fn label(m: &Message) -> (Kind, u16) {
            NewNode::label(m)
        }
    }

    fn edges(recs: &[WordRec], bit: u16) -> usize {
        let mut prev = 0u16;
        let mut n = 0;
        for &(.., w) in recs {
            if w & bit != 0 && prev & bit == 0 {
                n += 1;
            }
            prev = w;
        }
        n
    }

    /// 相手の入力ワードで `bit` が立った最初と最後の間に、`bit` の立っていないフレームがあるか。
    fn has_gap(recs: &[WordRec], bit: u16) -> bool {
        let on: Vec<bool> = recs.iter().map(|r| r.3 & bit != 0).collect();
        match (on.iter().position(|&x| x), on.iter().rposition(|&x| x)) {
            (Some(a), Some(b)) => on[a..=b].iter().any(|&x| !x),
            _ => false,
        }
    }

    fn late_words(sender: &ScreenNode, s: u16, at: u32) -> usize {
        sender.sent.iter().filter(|r| r.1 == s && r.0 > at).count()
    }

    const UP: u16 = keys::UP;

    #[test]
    fn a_held_direction_across_a_one_seat_decision_presses_once() {
        for delay in [1u32, 10, 16, 40, 100] {
            let hold = |f: u32| if (10..=60).contains(&f) { UP } else { 0 };
            let mut h = ScreenNode::new(Role::Host, Box::new(hold), false);
            let mut g = ScreenNode::new(Role::Guest, Box::new(hold), false);
            let mut presses = |r: Role, t: u32, _: u16| match (r, t) {
                (Role::Host, 320) => Some((D, 0)),
                _ => None,
            };
            let cfg = Cfg { end: 2000, ..CFG };
            run(&mut h, &mut g, &cfg, &mut fixed(delay), &mut presses);
            assert_eq!(h.log.len(), 1, "片道 {delay}");
            check_logs(&h.log, &g.log, true).unwrap();
            assert_eq!((h.stays, g.stays), (1, 1), "同じ画面に残る step");
            assert_eq!(edges(&g.sent, UP), 1);
            assert_eq!(
                edges(&h.got, UP),
                1,
                "ホストが見るゲストの上（片道 {delay}）"
            );
            assert!(
                !has_gap(&h.got, UP),
                "ホストが見るゲストの上に空き（片道 {delay}）"
            );
            assert_eq!(edges(&h.sent, UP), 1);
            assert_eq!(
                edges(&g.got, UP),
                1,
                "ゲストが見るホストの上（片道 {delay}）"
            );
            assert!(
                !has_gap(&g.got, UP),
                "ゲストが見るホストの上に空き（片道 {delay}）"
            );
            if delay >= 16 {
                assert!(late_words(&g, 0, h.applied_at[0]) > 0, "片道 {delay}");
            }
        }
    }

    #[test]
    fn taps_around_a_step_that_stays_on_the_screen_are_not_lost() {
        type Script = fn(Role, u32) -> Pr;
        let host_decides: Script = |r, t| (r == Role::Host && t == 320).then_some((D, 0));
        let guest_decides: Script = |r, t| (r == Role::Guest && t == 328).then_some((D, 0));
        let host_decides_and_cancels: Script = |r, t| match (r, t) {
            (Role::Host, 320) => Some((D, 0)),
            (Role::Host, 800) => Some((C, 0)),
            _ => None,
        };
        for (name, script, steps) in [
            ("ホストの決定", host_decides, 1usize),
            ("ゲストの決定", guest_decides, 1),
            ("ホストの決定と取り消し", host_decides_and_cancels, 2),
        ] {
            for delay in [1u32, 10, 16, 40, 100] {
                let tap = |f: u32| {
                    if (14..=70).contains(&f) && f.is_multiple_of(2) {
                        UP
                    } else {
                        0
                    }
                };
                let mut h = ScreenNode::new(Role::Host, Box::new(tap), false);
                let mut g = ScreenNode::new(Role::Guest, Box::new(tap), false);
                let mut presses = |r: Role, t: u32, _: u16| script(r, t);
                let cfg = Cfg { end: 2000, ..CFG };
                run(&mut h, &mut g, &cfg, &mut fixed(delay), &mut presses);
                let at = format!("{name}・片道 {delay}");
                assert_eq!(h.log.len(), steps, "{at}");
                check_logs(&h.log, &g.log, true).unwrap();
                assert_eq!(
                    (h.stays, g.stays, h.moves),
                    (steps as u32, steps as u32, 0),
                    "{at}"
                );
                assert_eq!(edges(&g.sent, UP), 29);
                assert_eq!(edges(&h.got, UP), 29, "ホストが見るゲストの押下（{at}）");
                assert_eq!(edges(&g.got, UP), 29, "ゲストが見るホストの押下（{at}）");
            }
        }
    }

    #[test]
    fn random_runs_pass_carried_words_once_in_order_within_a_screen() {
        let unique = |f: u32| 0x8000 | (f & 0x7FFF) as u16;
        let (mut words, mut carried, mut stays, mut moves) = (0usize, 0usize, 0u32, 0u32);
        for seed in 0..300u64 {
            let mut rng = Rng(seed ^ 0x0A0B_0C0D);
            let cfg = random_cfg(&mut rng, RANDOM_END);
            let mut h = ScreenNode::new(Role::Host, Box::new(unique), true);
            let mut g = ScreenNode::new(Role::Guest, Box::new(unique), true);
            run(
                &mut h,
                &mut g,
                &cfg,
                &mut chaos_faults(seed),
                &mut chaos_presses(seed),
            );
            if let Err(e) = check_logs(&h.log, &g.log, true) {
                panic!("seed {seed}: {e}");
            }
            for (who, sender, receiver) in [("host ← guest", &g, &h), ("guest ← host", &h, &g)]
            {
                match stream_ok(&sender.sent, &receiver.got) {
                    Ok((n, c)) => {
                        words += n;
                        carried += c;
                    }
                    Err(e) => panic!("seed {seed} {who}: {e}"),
                }
            }
            stays += h.stays;
            moves += h.moves;
        }
        eprintln!(
            "同じ画面に残る step の乱数の実行 (300 seed): 確かめた語 {words}、前の step から続けて渡した語 {carried}、同じ画面に残る step {stays}、画面が移る step {moves}"
        );
        assert!(words > 50_000 && carried > 5_000 && stays > 1_000 && moves > 1_000);
    }

    /// 受け手が渡した相手の入力ワード（0 と直前の入力ワードの繰り返しを除く）が、送り手の積んだ入力ワードの並びの部分列で、渡したときの画面の区切りが
    /// 積んだときと同じ。（確かめた入力ワードの数, 前の step の入力ワードを次の step で渡した数）を返す。
    fn stream_ok(sent: &[WordRec], got: &[WordRec]) -> Result<(usize, usize), String> {
        let at: HashMap<u16, (usize, u16, u32)> = sent
            .iter()
            .enumerate()
            .map(|(i, &(_, s, seg, w))| (w, (i, s, seg)))
            .collect();
        let (mut last, mut prev, mut n, mut carried) = (None::<usize>, 0u16, 0usize, 0usize);
        for &(t, rs, rseg, w) in got {
            if w == 0 || w == prev {
                prev = w;
                continue;
            }
            prev = w;
            let Some(&(i, s, seg)) = at.get(&w) else {
                return Err(format!("{t} ms の語 {w:#06x} を送り手は積んでいない"));
            };
            if last.is_some_and(|l| i <= l) {
                return Err(format!("{t} ms の語 {w:#06x} で順が戻った / 2 度渡した"));
            }
            if seg != rseg {
                return Err(format!(
                    "{t} ms の語 {w:#06x} は区切り {seg} の語なのに、区切り {rseg} で渡した"
                ));
            }
            last = Some(i);
            n += 1;
            if rs > s {
                carried += 1;
            }
        }
        Ok((n, carried))
    }
}
