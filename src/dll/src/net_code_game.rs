//! ゲーム内メニューの見張り（ini の `code_side = game`）。
//!
//! `offer` / `answer` の見張り（`net_code.rs` の `Watcher`）は、起動のときに側が決まり、ファイルを 250 ms ごとに読む。こちらは
//! **側を決めずに待ち**、ゲームのスレッドのメニューからの指示（`net_ui::UiLink`）で動く。ファイルは使わず、コードは
//! クリップボード（`clipboard::Clipboard`。開発用は `clipboard = file`）で受け渡す:
//! - **作る**: 始める側として自分のコード（秘密と session は作るたびに OS の乱数で引く）を作り、クリップボードへコピーする。
//! - **貼る**: クリップボードを読む。文字数が短すぎれば（`code::MIN_CODE_CHARS` 未満）読まずに断る。自分のコードがまだ無ければ
//!   返す側として読み（返すコードをクリップボードへコピーする）、あれば相手の返すコードとして読む（`Swapper::on_peer_file`。
//!   コードの正しさの判定は `higanr_core::code::read_peer`）。読めたらセッションの設定を `LinkSlot` へ置く（ゲームのスレッドが
//!   次の tick でセッションを作り、繋ぎ始める）。
//! - **取りやめる**: 待ちをやめる。セッションをもう作っていたら、候補の無い設定を置いて繋がらないセッションに替える。
//! - **ルームで待つ**: メニューの入力で決めたルーム名（`UiLink::room_name`）で、ルームサーバーへ繋ぐ（`net_room::RoomConn`。受信は
//!   専用のスレッド `higanr-room`、送りと手順の判断はこの見張り）。決めた名は次の初めの値として覚える（`room_last`）。
//!   始める側で相手がまだ来ていない間にルームの期限が切れたら、同じ名で自動で入り直す（`room_rejoin`）。ルームに先に入った側
//!   （`room::Role::Offer`）は「作る」と同じに `Swapper::offer` でコードを作ってルームへ渡し、相手のコードを受けたらセッションの設定を置く。
//!   後に入った側（`Answer`）は相手のコードを読み、返すコードをルームへ渡せた時点（`submit_code` の `Ok`）でセッションの設定を置く
//!   （サーバーの 1000 は待たない）。**どちらの側もクリップボードへはコピーしない**（人のクリップボードの中身を黙って替えない）。
//!   ルームで待つ間の諦めはルームの期限（10 分。サーバーが 4408 で切る）に任せ、`check_window` の 30 分は数えない。
//!   ルーム名は画面にだけ出し、本ログには書かない。
//! - 対戦の形の値（`UiLink::shape`）は、セッションの設定を置く直前に読んで HELLO で名乗る形に使う（`ModeCtx::choose_pick`）。
//!   置いた後に変えられたら「この接続では変わりません」の出来事（`Event::ShapeLate`）を返す。
//!
//! 見張り → メニューは、文字列でなく「種類（`Event` / `CodeState`）＋数」の `Note` を `UiLink::publish` で渡す。
//! 相手の名前だけは文字の表に無い文字を `?` に直してから載せる（`sanitize_name`）。
//!
//! 繋がったのを見たら、本ログへ「繋がった」と決まった形を残し、知らせ（`Notify`。音と点滅）を 1 回呼んで終わる。
//! ゲームのスレッドはこのスレッドに触らない（指示のリングバッファと状態の入れ物だけ）。遅延の増分は 0（繋がる前のメニューの間だけ動く）。
#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use higanr_core::auth::Secret;
use higanr_core::candidate::Candidates;
use higanr_core::code;
use higanr_core::delay_pick::DelayChoice;
use higanr_core::link;
use higanr_core::mode_pick::Pick;
use higanr_core::nat::NatMark;
use higanr_core::net_menu::{
    CodeState, Event, NAME_UNITS, Note, ROOM_UNITS, RoomNameWhy, sanitize_name, sanitize_room,
};
use higanr_core::overlay::ui_glyphs;
use higanr_core::room::{self, Failure, Role, RoomName};
use higanr_core::room_input::RoomBuf;
use higanr_core::room_queue::{AssignedRoom, QueueEvent};
use higanr_core::room_rejoin::{Situation, should_rejoin, should_requeue};
use higanr_core::seat_pick::SeatAsk;
use higanr_core::spectate::MAX_SPECTATORS;
use higanr_core::update::BUILD_NUMBER;
use higanr_core::watch_list::{Posting, WatchName};
use higanr_core::wire::{Arrival, Mode, Seat};

/// メニューの形式・遅延の値（`UiLink::shape` と `UiLink::start_delay`）。セッションの設定を置いたときの値と今の値を比べて、置いた後に変えられたかを見る。
type MenuShape = (Option<Pick>, Option<DelayChoice>);

use crate::clipboard::Clipboard;
use crate::net_code::{
    Kind, ModeCtx, Notify, Say, Shared, Step, Swapper, Timing, Watch, addrs_text, connected_say,
    decided_shape_log, draw_session,
};
use crate::net_config::{CodeSide, NetSettings};
use crate::net_room::{QueueConn, RoomConn, RoomOptions, SubmitRefused, code_side};
use crate::net_udp::{self, GameNet, RecvThread};
use crate::net_ui::{Order, UiLink};
use crate::netplay::{LinkSlot, NetPlay};
use crate::room_last;
use crate::spectate_offer::{self, ListingInfo, OfferKit, Parts};
use crate::trial_summary::Refusal;

/// 見張りが指示を見に来る間隔（指示が無い間は原子変数を 1 つ読むだけ）。
const GAME_POLL: Duration = Duration::from_millis(50);

pub struct GameUi {
    pub link: Arc<UiLink>,
    pub clip: Box<dyn Clipboard>,
    /// ルームサーバーへ繋ぐ設定（ini の `room_server` から）。None なら対戦の「ルームマッチ」の指示は来ない（メニューが項目を出さない）。
    pub room: Option<RoomOptions>,
    /// 前に使ったルーム名を覚えるファイルの置き場（`room_last`。接続コードの置き場 `code_dir`。None なら覚えない）。
    /// 書くのは見張りのスレッドだけ。
    pub room_last_dir: Option<PathBuf>,
    /// この起動の前のつながりの session。同じ session のコードを貼られたら読まない（`Event::Spent`）: 片づけた後もクリップボードには
    /// 前の相手のコードが残りうるので、そのまま貼ると前の相手（もう別の受信ソケット・別のコードで待っている）へつなぎに行き、
    /// 30 分の期間まで待ち続ける。最初のつながりは空。
    pub spent: Vec<u32>,
}

struct RoomRun {
    conn: RoomConn,
    /// 入ったルームの名（入り直しで同じ名を使う。`RoomName` は名を `Debug` に出すので本ログに `{:?}` で書かない）。
    name: RoomName,
    /// ルームで決まった側（`Joined` の役割を `net_room::code_side` でコピーしたもの。来る前は None）。
    side: Option<CodeSide>,
    /// 相手がルームに来た（`room::Event::Matched`。来た後の期限切れは入り直さない —— `room_rejoin`）。
    peer_came: bool,
    /// 始める側: ルームへ渡した自分のコード（相手の返すコードをこれで読む）。
    sw: Option<Swapper>,
    /// セッションの設定を置いた（以後のルームの手順の失敗は画面に出さず、繋がるかは P2P の側に任せる）。
    placed: bool,
}

pub(crate) struct GameWatcher {
    s: NetSettings,
    nonce: u64,
    own_cands: Candidates,
    /// 作った（始める側）か、返す側として読み始めた後の状態。無ければまだどちらでもない。
    sw: Option<Swapper>,
    /// 最後に読めた（セッションの設定を置いた）相手のコードの文字列。同じ文字列がもう一度貼られたら読み直さない。
    accepted: Option<String>,
    slot: Arc<LinkSlot>,
    shared: Arc<Shared>,
    timing: Timing,
    notify: Notify,
    mode: ModeCtx,
    ui: GameUi,
    glyphs: Vec<char>,
    t0: Instant,
    /// 最初の「作る」「貼る」から（使えるコードが届かないまま `timing.window` 経ったら待ちをやめる）。
    started: Option<Instant>,
    note: Note,
    placed_shape: Option<MenuShape>,
    room: Option<RoomRun>,
    /// ルームの経路を作り直した回数（1 回の見回りの途中でルームの経路が入れ替わったら、`room_tick` が古いルームの出来事を捨てるために見る）。
    room_gen: u32,
    /// 前の自動の入り直しの時刻（`room_rejoin::should_rejoin` の 60 秒の間隔。人が対戦の「ルームマッチ」を選び直すと忘れる）。
    last_rejoin: Option<Instant>,
    /// 観戦の配る側の材料（`[spectate] enable = 1` で、用意できたときだけ。つながったら `start_casting` が使って空にする）。
    spectate: Option<OfferKit>,
    /// 自分のコードに載せる回線の型の印（ネットワークを開いたときに決まる。`start_game`）。
    nat: NatMark,
    /// 対戦の「ルームマッチ」で相手のコードを読んでセッションの設定を置いたときのルーム名（観戦用のルームの名を導く元。`Debug` に名が出るので
    /// 本ログに `{:?}` で書かない）。ルームの経路でない操作では持たない。
    met_room: Option<RoomName>,
    /// 相手の名前（相手のコードに載っていたもの。一覧に載せる 1P・2P の名前に使う）。コードを読んだ後だけ。
    peer_name: Option<higanr_core::handshake::Name>,
    /// ランダム対戦の待ち列の手順（待ち列にいる間だけ。組になったら割り当てのルームの `room` へ替わる）。
    queue: Option<QueueConn>,
    /// ランダム対戦の経路にいる間（待ち列から、セッションの設定を置いた後まで）の、待ち列に入った順。待ち列にいる間と、まだ順が届かない間は
    /// `Unknown`。席の希望がぶつかったとき、先に待ち列に入った側を優先するのに使う（`place`）。
    random_order: Option<Arrival>,
    /// 前の待ち列の入り直しの時刻（`room_rejoin::should_requeue` の最短の間隔を見る。人が「ランダム対戦」を選び直すと忘れる）。
    last_requeue: Option<Instant>,
    /// 今のつながりがランダム対戦で会った相手。ホストなら、割り当てたルーム名から観戦用のルームの名を導いて観戦の一覧に載せる（`start_casting`）。
    random_met: bool,
    /// 待ち列の鍵のビルド番号（製品は `update::BUILD_NUMBER`。検査が替える）。
    build: u32,
}

impl GameWatcher {
    #[allow(clippy::too_many_arguments)] // 見張りの材料を 1 か所で受ける（束ねる型を足しても読み手は増えない）
    pub(crate) fn new(
        s: &NetSettings,
        nonce: u64,
        own_cands: Candidates,
        slot: Arc<LinkSlot>,
        shared: Arc<Shared>,
        timing: Timing,
        notify: Notify,
        mode: ModeCtx,
        ui: GameUi,
    ) -> GameWatcher {
        // 状態の札はメニューとのインターフェースに残っている値の続きから数える（つなぎ直しの 2 本目の見張りが前の見張りの札と同じ数を書かない）。
        let note = Note {
            seq: ui.link.note().map_or(0, |n| n.seq),
            ..Note::default()
        };
        GameWatcher {
            s: s.clone(),
            nonce,
            own_cands,
            sw: None,
            accepted: None,
            slot,
            shared,
            timing,
            notify,
            mode,
            ui,
            glyphs: ui_glyphs(),
            t0: Instant::now(),
            started: None,
            note,
            placed_shape: None,
            room: None,
            room_gen: 0,
            last_rejoin: None,
            spectate: None,
            nat: NatMark::UNKNOWN,
            met_room: None,
            peer_name: None,
            queue: None,
            random_order: None,
            last_requeue: None,
            random_met: false,
            build: BUILD_NUMBER,
        }
    }

    pub(crate) fn set_nat(&mut self, nat: NatMark) {
        self.nat = nat;
    }

    pub(crate) fn set_spectate(&mut self, kit: OfferKit) {
        self.spectate = Some(kit);
    }

    /// つながった: マッチのホストで、そのつながりが対戦の「ルームマッチ」かランダム対戦で会った相手で、観戦の用意があれば、観戦用のルームで観戦者を待つ
    /// スレッドを起こす（`spectate_offer`）。ランダム対戦のホストは、進行中のランダム対戦の一覧にも載せる（両者が `[spectate] enable = 1`
    /// のときだけ。`LinkSlot::may_list`）。どれかが欠けたら起こさず、理由を本ログへ
    /// （ルーム名は書かない）。見張りのスレッドで呼ぶ。
    pub(crate) fn start_casting(&mut self) {
        let Some(kit) = self.spectate.take() else {
            return;
        };
        let Some(room) = self.met_room else {
            self.log("観戦の配る側は起こさない（対戦のルームマッチ・ランダム対戦で会った相手ではない。観戦用のルームの名を導けない）");
            return;
        };
        match self.slot.is_host() {
            Some(true) => {}
            Some(false) => {
                self.log("観戦の配る側は起こさない（この試合のゲストなので、ホストが配る）");
                return;
            }
            None => {
                self.log("観戦の配る側は起こさない（試合の役が分からない）");
                return;
            }
        }
        let listing = if self.random_met {
            // ランダムマッチは一覧に載らなければ観戦用のルームの名を知る人がいないので、相手が断ったら配る側も起こさない。
            if self.slot.may_list() != Some(true) {
                self.log("観戦の配る側は起こさない（ランダムマッチの相手が一覧に載せない設定。[spectate] enable = 0）");
                return;
            }
            match self.listing_info(&room) {
                Some(l) => Some(l),
                None => {
                    self.log(
                        "観戦の配る側は起こさない（ランダム対戦の割り当てのルーム名を読めない）",
                    );
                    return;
                }
            }
        } else {
            None
        };
        let listed = listing.is_some();
        match spectate_offer::spawn(kit, &room, listing) {
            Ok(()) => self.log(if listed {
                "観戦の配る側を起こした・進行中のランダム対戦の一覧に載せる（観戦用のルームの名は割り当てのルーム名から導く。名は記録しない）"
            } else {
                "観戦の配る側を起こした（観戦用のルームの名はルーム名から導く。名は記録しない）"
            }),
            Err(e) => self.log_problem(format!("{e}（観戦だけを使わない。対戦は続く）")),
        }
    }

    /// 一覧に載せる材料: 席の順（1P・2P）に並べた両者の名前・合意した同期方式と遅延・観戦者の上限。名前は載せられる文字に直す
    /// （`WatchName::lossy`）。席が分からなければ自分を 1P とする。
    fn listing_info(&self, room: &RoomName) -> Option<ListingInfo> {
        let assigned = AssignedRoom::parse(room.as_str())?;
        let own = WatchName::lossy(&self.s.name);
        let peer = WatchName::lossy(
            &self
                .peer_name
                .map(|n| String::from_utf8_lossy(n.as_bytes()).into_owned())
                .unwrap_or_default(),
        );
        let (p1, p2) = match self.slot.seat() {
            Some(Seat::P2) => (peer, own),
            _ => (own, peer),
        };
        let (mode, delay) = self.slot.shape().unwrap_or((Mode::Delay, 0));
        Some(ListingInfo {
            build: self.build,
            posting: Posting {
                room: assigned,
                p1,
                p2,
                mode,
                delay,
                cap: u8::try_from(MAX_SPECTATORS).unwrap_or(u8::MAX),
            },
        })
    }

    fn say(&mut self, s: &Say) {
        let secs = self.t0.elapsed().as_secs_f32();
        self.shared.count(s.kind, secs);
        let line = format!("接続コード（{secs:.1} 秒）: {}", s.log);
        // 断った・受けない（`Kind::Refused`）は簡潔なログにも残す。
        if matches!(s.kind, Kind::Refused(_)) {
            crate::main_log::push(line.clone());
        }
        self.shared.note(line);
    }

    /// 失敗の文。「ネットワークの外」の行（詳細）に加えて、簡潔なログにもその場で 1 行出す。
    fn log_problem(&self, line: impl Into<String>) {
        let secs = self.t0.elapsed().as_secs_f32();
        let line = format!("接続コード（{secs:.1} 秒）: {}", line.into());
        crate::main_log::push(line.clone());
        self.shared.note(line);
    }

    fn log(&self, line: impl Into<String>) {
        let secs = self.t0.elapsed().as_secs_f32();
        self.shared
            .note(format!("接続コード（{secs:.1} 秒）: {}", line.into()));
    }

    fn event(&mut self, e: Event) {
        self.note.event = e;
        self.note.seq = self.note.seq.wrapping_add(1);
        self.ui.link.publish(&self.note);
    }

    fn set_own(&mut self, text: &str, now: u64) {
        let (head, tail) = Note::edges_of(text);
        self.note.code_len = text.chars().count() as u32;
        self.note.head = head;
        self.note.tail = tail;
        self.note.since = now as u32;
    }

    pub(crate) fn handle(&mut self, o: Order) {
        self.refresh_player_name();
        match o {
            Order::Create => self.create(),
            Order::Paste => self.paste(),
            Order::Cancel => self.cancel(),
            Order::Room => self.room_start(),
            Order::Random => self.random_start(),
            // 観戦者の入口。観戦者のネットワークを開く・観戦用のルームへ入るのは、この見張り（対戦者のネットワークの見張り）ではない: メニューの決定は
            // `UiOut::spectate_open` として入力の入口が受け（`script_win::request_spectate`。待っているだけの対戦者のネットワークはそこで
            // 片づけてから開く）、観戦者のルームのスレッド（`spectate_join`）が自分の指示のリングバッファの読み手になる。この指示が届くのは
            // 観戦者のネットワークを開く前の短い間だけなので、読み捨てる。
            Order::Spectate | Order::SpectateCancel => {}
        }
    }

    /// メニューで名前を決めていれば、以後に作る接続コード・ルームの HELLO はその名前で名乗る。名前は指示のリングバッファを通さず `UiLink` の入れ物に
    /// 置かれるので、指示を 1 つ行う前にここで読み直す。まだ決めていなければ ini・覚えたファイルの名前のまま。名前を変えられるのは
    /// 何も待っていない間だけなので、待ちの途中で名乗る名前が替わることは無い。
    fn refresh_player_name(&mut self) {
        if let Some(n) = self.ui.link.player_name()
            && self.s.name != n.as_str()
        {
            self.s.name = n.as_str().to_string();
        }
    }

    fn create(&mut self) {
        self.leave_room();
        self.started = Some(Instant::now());
        let now = net_udp::now_unix();
        let (secret, session) = match (net_udp::draw_secret(), draw_session()) {
            (Ok(sec), Ok(ses)) => (sec, ses),
            (Err(e), _) | (_, Err(e)) => {
                self.say(&Say::new(e, "").kind(Kind::Refused(Refusal::Internal)));
                self.event(Event::Internal);
                return;
            }
        };
        let sw = match self.offer_swapper(secret, session, now) {
            Ok(sw) => sw,
            Err(e) => {
                self.say(&Say::new(e, "").kind(Kind::Refused(Refusal::Internal)));
                self.event(Event::Internal);
                return;
            }
        };
        let text = sw.own_text().unwrap_or_default().to_string();
        match self.ui.clip.write(&text) {
            Ok(()) => {
                // 新しい相手を待つので、前の相手の回線の型の印は忘れる。
                self.slot.set_peer_nat(None);
                self.sw = Some(sw);
                self.accepted = None;
                self.placed_shape = None;
                self.set_own(&text, now);
                self.note.state = CodeState::Offered;
                self.note.name = [0; higanr_core::net_menu::NAME_UNITS];
                self.log(format!(
                    "接続コード（始める側。session 0x{session:08X}・{} 文字）を作り、クリップボードへコピーした（有効期限 {}）",
                    text.chars().count(),
                    code::span_text(code::DEFAULT_TTL_SECS)
                ));
                self.event(Event::Made);
            }
            Err(e) => {
                self.log_problem(format!("クリップボードへコピーできない: {e}"));
                self.event(Event::ClipWrite);
            }
        }
    }

    /// 始める側の交換役（自分の候補と回線の型の印を載せたコードを作る）。
    fn offer_swapper(&self, secret: Secret, session: u32, now: u64) -> Result<Swapper, String> {
        Swapper::offer_with_nat(
            &self.s,
            self.nonce,
            self.own_cands,
            self.nat,
            secret,
            session,
            now,
        )
    }

    fn answer_swapper(&self) -> Swapper {
        Swapper::answer(&self.s, self.nonce, self.own_cands).with_nat(self.nat)
    }

    fn paste(&mut self) {
        self.leave_room();
        self.started.get_or_insert_with(Instant::now);
        let text = match self.ui.clip.read() {
            Ok(Some(t)) => t,
            Ok(None) => {
                self.note.paste_len = 0;
                self.log("クリップボードが空");
                self.event(Event::Empty);
                return;
            }
            Err(e) => {
                self.log_problem(format!("クリップボードを読めない: {e}"));
                self.event(Event::Unreadable);
                return;
            }
        };
        // 前後の空白（改行）と、メモ帳が付ける BOM は文字数に数えない（`code` が同じものを読み飛ばす）。
        let len = bare(&text).chars().count();
        self.note.paste_len = len as u32;
        if len < code::MIN_CODE_CHARS {
            self.log(format!(
                "貼られた文字が短すぎる（{len} 文字。接続コードは {} 文字以上）",
                code::MIN_CODE_CHARS
            ));
            self.event(Event::Short);
            return;
        }
        let now = net_udp::now_unix();
        // 自分のコードがまだ無ければ、返す側として読む（この貼り付けで返す側を作った。断ったら捨てて、まだどちらでもない形へ戻す）。
        let created_answer = self.sw.is_none();
        let mut sw = self.sw.take().unwrap_or_else(|| self.answer_swapper());
        // 断った文字をもう一度貼られたら、黙らず同じ断りをもう一度言う（読めた文字だけ「もう読んでいます」にする）。
        if self.accepted.as_deref() != Some(bare(&text)) {
            sw.forget_peer_seen();
        }
        // `Swapper` は中身が同じ文字かをバイト列で見る。改行や BOM の違いで別の文字にならないよう、除いた文字を渡す。
        match sw.on_peer_file(Some(bare(&text).as_bytes()), now) {
            Step::Same => {
                self.keep(sw);
                self.on_same();
            }
            Step::Refused(say) => {
                self.say(&say);
                let ev = event_of(say.kind);
                // バージョン違いは、メニューの行に相手のビルド番号を出す（`Note::peer_ver`）。ビルド違いは相手のビルドを出す。
                if let Kind::Refused(Refusal::OtherVersion { theirs }) = say.kind {
                    self.note.peer_ver = theirs;
                }
                if let Kind::Refused(Refusal::OtherBuild { theirs }) = say.kind {
                    self.note.peer_build = theirs;
                }
                if !created_answer {
                    self.keep(sw);
                }
                self.event(ev);
            }
            Step::Ready {
                link,
                my_code: _,
                note: _,
            } if self.ui.spent.contains(&link.handshake.session_id) => {
                // 前のつながりで使ったコード（session が同じ）。セッションの設定は置かない。この貼り付けで作った返す側は捨てる。
                self.log(format!(
                    "前のつながりで使ったコード（session 0x{:08X}）を貼られたので読まない",
                    link.handshake.session_id
                ));
                if !created_answer {
                    self.keep(sw);
                }
                self.event(Event::Spent);
            }
            Step::Ready {
                link,
                my_code,
                note,
            } => self.ready(sw, *link, my_code, &note, &text, now),
        }
    }

    fn keep(&mut self, sw: Swapper) {
        self.sw = Some(sw);
    }

    fn on_same(&mut self) {
        let reply = self
            .sw
            .as_ref()
            .filter(|s| s.side() == CodeSide::Answer)
            .and_then(|s| s.own_text())
            .map(str::to_string);
        match reply {
            Some(r) => match self.ui.clip.write(&r) {
                Ok(()) => {
                    self.log("同じ始める側のコードをもう読んでいる。返すコードをもう一度クリップボードへコピーした");
                    self.event(Event::ReplyMade);
                }
                Err(e) => {
                    self.log_problem(format!("クリップボードへコピーできない: {e}"));
                    self.event(Event::ClipWrite);
                }
            },
            None => {
                self.log("同じコードをもう読んでいる");
                self.event(Event::Same);
            }
        }
    }

    fn place(&mut self, sw: &Swapper, link: link::Config) {
        // 名乗る形は、セッションの設定を置く直前にここ（見張りのスレッド）で決める。メニューで選んだ形は原子変数から読むだけ。
        let pick = self.ui.link.shape();
        let wish = match self.mode.choose_pick(pick) {
            Some((choice, lines)) => {
                for l in lines {
                    self.log(l);
                }
                Some(choice.wish)
            }
            None => None,
        };
        // 席の希望: メニューの「サイド」（変えていなければメニューの初めの値 = ini の `host_seat`、無ければおまかせ）と、始める側か
        // 返す側か。ルームの経路では先にルームに入った人が始める側、クリップボードの経路では「接続コードを作る」を選んだ人が始める側。
        let seat = SeatAsk {
            wish: self
                .ui
                .link
                .seat()
                .unwrap_or_else(|| crate::script_win::initial_seat(&self.s)),
            // 見張りの交換役（`Swapper`）は始める側か返す側のどちらか。ほかの値は来ないが、来たら名乗らない（ホストを先と見る）。
            // ランダム対戦は待ち列に先に入った側が先（ルームに先に着いた側ではない）。
            arrival: self
                .random_order
                .filter(|a| *a != Arrival::Unknown)
                .unwrap_or(match sw.side() {
                    CodeSide::Offer => Arrival::First,
                    CodeSide::Answer => Arrival::Second,
                    CodeSide::Spectate | CodeSide::Game => Arrival::Unknown,
                }),
        };
        self.log(format!(
            "席の希望を名乗る: {:?}（{:?}。First = ルームに先にいた・コードを作った側）",
            seat.wish, seat.arrival
        ));
        // つなぐ前に決めた遅延（メニューか ini の auto）は、セッションの設定の前に置く（ゲームのスレッドが繋がった tick に読む）。
        let named = wish.map_or(Mode::Delay, |w| w.mode);
        let start = self.mode.start_delay(self.ui.link.start_delay(), named);
        self.slot.set_start(start);
        self.log(format!("つなぐ前の遅延: {start:?}（None は決めていない）"));
        self.slot.set_peer_nat(sw.peer_nat());
        // 一覧に載せてよいかを HELLO で名乗る（ランダムマッチのホストは両者が載せてよいときだけ載せる —— `start_casting`）。
        self.slot.set_list_wish(self.s.spectate.is_some());
        self.slot.offer_with_seat(link, wish, Some(seat));
        self.placed_shape = Some((pick, self.ui.link.start_delay()));
        let glyphs = &self.glyphs;
        let peer_name = sw
            .peer_name()
            .unwrap_or(higanr_core::handshake::Name::EMPTY);
        self.note.name = sanitize_name(peer_name.as_bytes(), |c| glyphs.binary_search(&c).is_ok());
        self.peer_name = Some(peer_name);
    }

    fn ready(
        &mut self,
        sw: Swapper,
        link: link::Config,
        my_code: Option<String>,
        note: &Say,
        text: &str,
        now: u64,
    ) {
        self.place(&sw, link);
        self.accepted = Some(bare(text).to_string());
        let side = sw.side();
        self.keep(sw);
        self.say(note);
        match (side, my_code) {
            (CodeSide::Answer, Some(reply)) => {
                self.set_own(&reply, now);
                self.note.state = CodeState::Replied;
                match self.ui.clip.write(&reply) {
                    Ok(()) => self.event(Event::ReplyMade),
                    Err(e) => {
                        self.log_problem(format!("クリップボードへコピーできない: {e}"));
                        self.event(Event::ClipWrite);
                    }
                }
            }
            _ => {
                self.note.state = CodeState::Linking;
                self.event(Event::PeerRead);
            }
        }
    }

    /// 待っているコード（作った・読んだ）を忘れる。セッションをもう作ってあれば、候補の無い設定で置き直して繋がらないセッションに替える
    /// （繋ぎ始めたセッションは止められない）。
    fn forget_code(&mut self) {
        let had_link = self.slot.generation() > 0;
        self.slot.set_peer_nat(None);
        self.sw = None;
        self.accepted = None;
        self.started = None;
        self.placed_shape = None;
        if had_link {
            let session = draw_session().unwrap_or(0);
            let secret = net_udp::draw_secret().unwrap_or(Secret([0; 16]));
            self.slot.offer_with_mode(
                link::Config {
                    handshake: self.s.handshake_config(session, self.nonce),
                    secret,
                    peers: Candidates::EMPTY,
                    // 候補の無いセッション（何も送らず何も受けない）なので、種は使われない。それでも決まった値にしない。
                    key_seed: net_udp::draw_key_seed()
                        .unwrap_or(higanr_core::seal::KeySeed([0; 32])),
                },
                None,
            );
        }
    }

    fn cancel(&mut self) {
        self.leave_room();
        self.forget_code();
        self.note.state = CodeState::Idle;
        self.note.name = [0; NAME_UNITS];
        self.log("待ちを取りやめた");
        self.event(Event::Cancelled);
    }

    // ---- ルームの経路 ----

    /// ルームの経路をやめる（作る・貼る・取りやめるの始まり）。ルームの状態だったら `Idle` へ戻す（その後の操作が状態を決める）。
    /// 置いたセッションの設定はそのまま（作る・貼るが置き直すか、取りやめるが繋がらないセッションに替える）。
    fn leave_room(&mut self) {
        // ルームで会った相手ではなくなる（作る・貼る・取りやめる・別のルーム名）。観戦の配る側は元のルーム名を失う。
        self.met_room = None;
        self.random_met = false;
        if let Some(mut q) = self.queue.take() {
            q.cancel();
            self.log("ランダム対戦の待ち列をやめた（別の操作が選ばれた）");
        }
        if let Some(mut r) = self.room.take() {
            r.conn.cancel();
            self.log("ルームの待ちをやめた（別の操作が選ばれた）");
        }
        self.random_order = None;
        self.note.random = false;
        if self.note.state.is_room() {
            self.note.state = CodeState::Idle;
        }
    }

    fn room_end(&mut self, ev: Event) {
        if let Some(mut r) = self.room.take() {
            r.conn.cancel();
        }
        self.note.state = CodeState::Idle;
        self.event(ev);
    }

    fn room_name_bad(&mut self, why: RoomNameWhy, reason: &str) {
        self.note.room_why = why;
        self.log(format!(
            "ルーム名を断った（{reason}。ルーム名は記録しない）"
        ));
        self.event(Event::RoomNameBad);
    }

    /// 前に使ったルーム名を覚える（置き場へ一時ファイル → 置き換え。名は本ログに書かない）。
    fn remember_room(&self, name: &RoomBuf) {
        let Some(dir) = self.ui.room_last_dir.as_deref() else {
            return;
        };
        match room_last::write(dir, name) {
            Ok(()) => self.log("前に使ったルーム名を覚えた（名は記録しない）"),
            Err(e) => self.log(format!("前に使ったルーム名を覚えられない: {e}")),
        }
    }

    /// 対戦の「ルームマッチ」: メニューの入力で決めたルーム名で、ルームサーバーへ繋ぎ始める（待たない）。
    fn room_start(&mut self) {
        let Some(opts) = self.ui.room.clone() else {
            self.log_problem(
                "ルームサーバーの設定（room_server）が無いのに、対戦のルームマッチが選ばれた",
            );
            self.event(Event::Internal);
            return;
        };
        let Some(buf) = self.ui.link.room_name() else {
            // メニューは名を置いてから指示を積む（`UiLink::send`）ので、来ない見込み。
            return self.room_name_bad(RoomNameWhy::Empty, "ルーム名が置かれていない");
        };
        // メニューも同じガード（`RoomName::parse`）で見て渡すが、ここでも見直す（断った理由は名を含まない）。
        let name = match buf.parse() {
            Ok(n) => n,
            // `RoomNameError` の Debug は位置と数だけで、名の文字を含まない。
            Err(e) => return self.room_name_bad(RoomNameWhy::of(e), &format!("{e:?}")),
        };
        if let Some(r) = self.room.as_ref().filter(|r| !r.placed) {
            if r.name == name && self.random_order.is_none() {
                // 同じ名でもうルームで待っている: 読み捨てる（入り直すと、先に入って待っている側の席を手放す）。
                self.log("もう同じルームで待っているので、ルームマッチの指示を読み捨てた");
                return;
            }
            self.leave_room();
        }
        if self.queue.is_some() || self.random_order.is_some() {
            // ランダム対戦の待ち（待ち列・割り当てのルーム・置いた後）から、名前つきのルームへ移る。
            self.leave_room();
        }
        // コードの経路からルームの経路へ移る: 作った・読んだコードを忘れ、置いたセッションがあれば繋がらないセッションに替える。
        self.forget_code();
        let conn = match RoomConn::start(&opts, &name) {
            Ok(c) => c,
            Err(e) => {
                self.log_problem(e);
                self.event(Event::Internal);
                return;
            }
        };
        // 名を覚えるのは、サーバーへつなぎ始められた後（つなぎ始められなかった名＝打ち間違い・始められない設定の名を、次の入力の
        // 初めの値にしない）。
        self.remember_room(&buf);
        let glyphs = &self.glyphs;
        self.note.room = sanitize_room(name.as_str(), |c| glyphs.binary_search(&c).is_ok());
        // 待ち始めた時刻（左上の「待った時間」の起点。期限が切れて自動で入り直しても変えない）。
        self.note.since = net_udp::now_unix() as u32;
        self.last_rejoin = None;
        self.note.name = [0; NAME_UNITS];
        self.note.state = CodeState::RoomJoining;
        self.room_gen = self.room_gen.wrapping_add(1);
        self.room = Some(RoomRun {
            conn,
            name,
            side: None,
            peer_came: false,
            sw: None,
            placed: false,
        });
        self.log("ルームサーバーへつなぎ始めた（ルーム名はメニューで打った名。名は記録しない）");
        self.event(Event::RoomJoining);
    }

    /// ルームの期限が切れたので、同じルーム名で自動で入り直す（判断は `room_rejoin::should_rejoin`）。状態は「待っている」のまま。
    /// 入り直しがすぐ失敗したら、今までの失敗の扱いに任せる。
    fn room_rejoin(&mut self) {
        let Some(mut old) = self.room.take() else {
            return;
        };
        old.conn.cancel();
        let name = old.name;
        let Some(opts) = self.ui.room.clone() else {
            return self.room_end(Event::Internal);
        };
        match RoomConn::start(&opts, &name) {
            Ok(conn) => {
                self.last_rejoin = Some(Instant::now());
                self.room_gen = self.room_gen.wrapping_add(1);
                self.room = Some(RoomRun {
                    conn,
                    name,
                    side: None,
                    peer_came: false,
                    sw: None,
                    placed: false,
                });
                self.log(
                    "ルームの期限が切れたので、同じルーム名で入り直した（ルーム名は記録しない）",
                );
            }
            Err(e) => {
                self.log_problem(e);
                self.room_end(Event::Internal);
            }
        }
    }

    // ---- ランダム対戦の経路 ----
    //
    // 待ち列（`room_queue`）に入り、組になったら割り当てられたルームへ入り直して、あとは名前つきのルームと同じ手順
    // （offer / answer の受け渡し → セッションの設定）。割り当てられたルーム名は画面にもログにも出さず、前に使った名としても覚えず、
    // 観戦用のルームの名の元にもしない。

    /// 「ランダム対戦」: 待ち列へ繋ぎ始める（待たない）。
    fn random_start(&mut self) {
        let Some(opts) = self.ui.room.clone() else {
            self.log_problem(
                "ルームサーバーの設定（room_server）が無いのに、ランダム対戦が選ばれた",
            );
            self.event(Event::Internal);
            return;
        };
        if self.random_order.is_some()
            && (self.queue.is_some() || self.room.as_ref().is_some_and(|r| !r.placed))
        {
            // もう待っている: 読み捨てる（入り直すと、先に待ち列に入った順を手放す）。
            self.log("もうランダム対戦で待っているので、ランダム対戦の指示を読み捨てた");
            return;
        }
        // 名前つきのルームや前のコードの経路から移る。
        self.leave_room();
        self.forget_code();
        let queue = match QueueConn::start_queue(&opts, self.build) {
            Ok(q) => q,
            Err(e) => {
                self.log_problem(e);
                self.event(Event::Internal);
                return;
            }
        };
        self.queue = Some(queue);
        self.random_order = Some(Arrival::Unknown);
        self.last_requeue = None;
        self.note.random = true;
        self.note.room = [0; ROOM_UNITS];
        self.note.name = [0; NAME_UNITS];
        // 待ち始めた時刻（左上の「待った時間」の起点。待ち列の期限が切れて自動で入り直しても変えない）。
        self.note.since = net_udp::now_unix() as u32;
        self.note.state = CodeState::RoomJoining;
        self.room_gen = self.room_gen.wrapping_add(1);
        self.log("ランダム対戦の待ち列へつなぎ始めた（ルーム名は割り当てられる。名は記録しない）");
        self.event(Event::RoomJoining);
    }

    /// ランダム対戦の待ちを失敗として終える（待ち列の接続も片づける）。
    fn random_end(&mut self, ev: Event) {
        if let Some(mut q) = self.queue.take() {
            q.cancel();
        }
        self.room_end(ev);
    }

    /// 待ち列へ入り直す（待ち列の期限切れ・割り当てのルームで相手が揃わなかった）。待機は続くので状態は変えない。
    fn random_requeue(&mut self, why: &str) {
        if let Some(mut r) = self.room.take() {
            r.conn.cancel();
        }
        if let Some(mut q) = self.queue.take() {
            q.cancel();
        }
        let Some(opts) = self.ui.room.clone() else {
            return self.random_end(Event::Internal);
        };
        match QueueConn::start_queue(&opts, self.build) {
            Ok(q) => {
                self.queue = Some(q);
                self.random_order = Some(Arrival::Unknown);
                self.last_requeue = Some(Instant::now());
                self.room_gen = self.room_gen.wrapping_add(1);
                self.note.name = [0; NAME_UNITS];
                self.note.state = CodeState::RoomWaiting;
                self.log(format!("{why}ので、ランダム対戦の待ち列へ入り直した"));
                self.event(Event::RoomWaiting);
            }
            Err(e) => {
                self.log_problem(e);
                self.random_end(Event::Internal);
            }
        }
    }

    fn queue_tick(&mut self) {
        let Some(q) = self.queue.as_mut() else {
            return;
        };
        let evs = q.poll();
        let notes = q.take_notes();
        let gen_now = self.room_gen;
        for n in notes {
            self.log_problem(format!("ランダム対戦の待ち列の運び: {n:?}"));
        }
        for e in evs {
            if self.room_gen != gen_now {
                // この見回りの途中で経路が入れ替わった（入り直し・組になった）。残りは古い待ち列の出来事なので捨てる。
                break;
            }
            match e {
                QueueEvent::Queued { expires_at } => self.queue_joined(expires_at),
                QueueEvent::Paired { room, first } => self.queue_paired(room, first),
                QueueEvent::Failed(f) => self.queue_failed(f),
            }
        }
        if self.queue.as_ref().is_some_and(|q| q.is_finished()) {
            self.queue = None;
        }
    }

    fn queue_joined(&mut self, expires_at: u64) {
        let now = net_udp::now_unix();
        let ttl = higanr_core::room_queue::RANDOM_QUEUE_TTL_MS / 1000;
        // 時計のずれで残りが 0 以下か期限より長く見えたら、期限で代える（表示の目安。切るのはサーバーと core）。
        let left = expires_at
            .checked_sub(now)
            .filter(|l| *l > 0 && *l <= ttl)
            .unwrap_or(ttl);
        self.note.state = CodeState::RoomWaiting;
        self.log(format!(
            "ランダム対戦の待ち列に入った（待ち列の期限まで {left} 秒）"
        ));
        self.event(Event::RoomWaiting);
    }

    fn queue_paired(&mut self, room: AssignedRoom, first: bool) {
        self.queue = None;
        let Some(opts) = self.ui.room.clone() else {
            return self.room_end(Event::Internal);
        };
        let name = room.name();
        match RoomConn::start(&opts, &name) {
            Ok(conn) => {
                self.random_order = Some(if first {
                    Arrival::First
                } else {
                    Arrival::Second
                });
                self.room_gen = self.room_gen.wrapping_add(1);
                self.room = Some(RoomRun {
                    conn,
                    name,
                    side: None,
                    peer_came: false,
                    sw: None,
                    placed: false,
                });
                self.log(format!(
                    "ランダム対戦の相手が決まった（待ち列に{}入った側）。割り当てのルームへ入る（ルーム名は記録しない）",
                    if first { "先に" } else { "後から" }
                ));
            }
            Err(e) => {
                self.log_problem(e);
                self.room_end(Event::Internal);
            }
        }
    }

    fn queue_failed(&mut self, f: Failure) {
        let since = self.last_requeue.map(|t| t.elapsed().as_secs());
        if should_requeue(f, false, since) {
            return self.random_requeue("待ち列の期限が切れた");
        }
        if let Failure::RateLimited { retry_after_secs } = f {
            self.note.retry_secs = u16::try_from(retry_after_secs).unwrap_or(u16::MAX);
        }
        self.log_problem(format!("ランダム対戦の待ち列で待てなかった: {f:?}"));
        self.random_end(room_failure_event(f));
    }

    fn room_tick(&mut self) {
        let Some(r) = self.room.as_mut() else {
            return;
        };
        let evs = r.conn.poll();
        let notes = r.conn.take_notes();
        let gen_now = self.room_gen;
        for n in notes {
            self.log_problem(format!("ルームの運び: {n:?}"));
        }
        for e in evs {
            if self.room_gen != gen_now {
                // この見回りの途中でルームの経路が入れ替わった（期限切れで入り直した）。残りは古いルームの出来事なので捨てる。
                break;
            }
            self.on_room_event(e);
        }
        if self.room.as_ref().is_some_and(|r| r.conn.is_finished()) {
            self.room = None;
        }
    }

    fn on_room_event(&mut self, e: room::Event) {
        match e {
            room::Event::Joined { role, expires_at } => self.room_joined(role, expires_at),
            room::Event::Matched { .. } => {
                if let Some(r) = self.room.as_mut() {
                    r.peer_came = true;
                }
                self.log("ルームに相手が来た（自分のコードが相手へ渡った）");
            }
            room::Event::PeerCode(code) => self.room_peer_code(&code),
            room::Event::Done => self.log("ルームの手順を終えた（サーバーが閉じた）"),
            room::Event::Failed(f) => self.room_failed(f),
        }
    }

    fn room_joined(&mut self, role: Role, expires_at: u64) {
        let now = net_udp::now_unix();
        let ttl = room::ROOM_TTL_MS / 1000;
        // サーバーの期限（UNIX 秒）をこちらの時計の期限へ。時計のずれで残りが 0 以下かルームの期限より長く見えたら、ルームの
        // 期限で代える（core の `room` の予備の期限と同じ考え。表示の目安で、切るのはサーバーと core）。
        let left = expires_at
            .checked_sub(now)
            .filter(|l| *l > 0 && *l <= ttl)
            .unwrap_or(ttl);
        self.note.state = CodeState::RoomWaiting;
        let side = code_side(role);
        if let Some(r) = self.room.as_mut() {
            r.side = Some(side);
        }
        self.log(format!(
            "ルームに入った（{}。ルームの期限まで {left} 秒）",
            side_text(side)
        ));
        self.event(Event::RoomWaiting);
        if side == CodeSide::Offer {
            self.room_offer(now);
        }
    }

    /// 始める側: 自分のコードを作ってルームへ渡す（クリップボードへはコピーしない）。
    fn room_offer(&mut self, now: u64) {
        let (secret, session) = match (net_udp::draw_secret(), draw_session()) {
            (Ok(sec), Ok(ses)) => (sec, ses),
            (Err(e), _) | (_, Err(e)) => {
                self.say(&Say::new(e, "").kind(Kind::Refused(Refusal::Internal)));
                return self.room_end(Event::Internal);
            }
        };
        let sw = match self.offer_swapper(secret, session, now) {
            Ok(sw) => sw,
            Err(e) => {
                self.say(&Say::new(e, "").kind(Kind::Refused(Refusal::Internal)));
                return self.room_end(Event::Internal);
            }
        };
        let text = sw.own_text().unwrap_or_default().to_string();
        let Some(r) = self.room.as_mut() else {
            return;
        };
        match r.conn.submit_code(&text) {
            Ok(()) => {
                r.sw = Some(sw);
                self.log(format!(
                    "接続コード（始める側。session 0x{session:08X}・{} 文字）を作り、ルームへ渡した（クリップボードにはコピーしない）",
                    text.chars().count()
                ));
            }
            // 送れなかった: 失敗の出来事は次の見回りで届く（そこで終える）。
            Err(SubmitRefused::NotSent) => {
                self.log_problem("始める側のコードをルームへ送れなかった")
            }
            Err(SubmitRefused::Core(e)) => {
                self.say(
                    &Say::new(format!("始める側のコードをルームが受けない: {e:?}"), "")
                        .kind(Kind::Refused(Refusal::Internal)),
                );
                self.room_end(Event::Internal);
            }
        }
    }

    /// ルームで相手のコードを受けた: 始める側は返すコードとして読んでセッションの設定を置く。返す側は読んで返すコードをルームへ渡し、
    /// 渡せた時点でセッションの設定を置く（サーバーの 1000 の前に切れても始める側にはコードが届いているかもしれないので、
    /// 繋がるかは P2P の側の時間切れに任せる）。
    fn room_peer_code(&mut self, text: &str) {
        let now = net_udp::now_unix();
        let Some(r) = self.room.as_mut() else {
            return;
        };
        let side = r.side;
        let mut sw = match (side, r.sw.take()) {
            (Some(CodeSide::Offer), Some(sw)) => sw,
            (Some(CodeSide::Answer), _) => self.answer_swapper(),
            _ => {
                self.log_problem("ルームで相手のコードを受けたが、自分の側かコードが無い");
                return self.room_end(Event::Internal);
            }
        };
        match sw.on_peer_file(Some(text.as_bytes()), now) {
            Step::Ready { link, my_code, .. } => {
                if side == Some(CodeSide::Answer) {
                    let Some(reply) = my_code else {
                        self.log_problem("返す側なのに返すコードができていない");
                        return self.room_end(Event::Internal);
                    };
                    let Some(r) = self.room.as_mut() else {
                        return;
                    };
                    match r.conn.submit_code(&reply) {
                        Ok(()) => self.log(format!(
                            "返すコード（{} 文字）をルームへ渡した（クリップボードにはコピーしない）",
                            reply.chars().count()
                        )),
                        Err(SubmitRefused::NotSent) => {
                            // 渡せていないのでセッションの設定は置かない（相手は返すコードを受けない）。失敗は次の見回りで届く。
                            self.log_problem(
                                "返すコードをルームへ送れなかった（セッションの設定は置かない）",
                            );
                            return;
                        }
                        Err(SubmitRefused::Core(e)) => {
                            self.log_problem(format!("返すコードをルームが受けない: {e:?}"));
                            return self.room_end(Event::Internal);
                        }
                    }
                }
                self.room_place(&sw, *link);
            }
            Step::Refused(say) => {
                self.say(
                    &Say::new(
                        format!("ルームで受けた相手のコードを読めない: {}", say.log),
                        "",
                    )
                    .kind(say.kind),
                );
                if let Kind::Refused(Refusal::OtherVersion { theirs }) = say.kind {
                    self.note.peer_ver = theirs;
                }
                if let Kind::Refused(Refusal::OtherBuild { theirs }) = say.kind {
                    self.note.peer_build = theirs;
                }
                self.room_end(room_refusal_event(say.kind));
            }
            Step::Same => {
                self.log("ルームで受けた相手のコードを、もう読んだと言われた");
                self.room_end(Event::Internal);
            }
        }
    }

    fn room_place(&mut self, sw: &Swapper, link: link::Config) {
        let addrs = addrs_text(&link.peers);
        self.place(sw, link);
        let side = self.room.as_mut().and_then(|r| {
            r.placed = true;
            r.side
        });
        // 観戦用のルームの名の元（ルームの経路を終えた後も、つながったときに使う）。ランダム対戦は割り当てのルーム名（画面・ログ・覚えるファイルには出さない）。
        self.random_met = self.random_order.is_some();
        self.met_room = self.room.as_ref().map(|r| r.name);
        self.note.state = CodeState::RoomLinking;
        self.say(
            &Say::new(
                format!(
                    "ルームで相手のコードを読み、セッションの設定を置いた（{}）。相手の候補 {addrs} へ繋いでいる",
                    side.map_or("側が分からない", side_text)
                ),
                "",
            )
            .kind(Kind::PeerRead),
        );
        self.event(Event::RoomPeer);
    }

    /// ルームの手順が終わらなかった。セッションの設定を置いた後なら画面には出さない（P2P の側に任せる）。
    fn room_failed(&mut self, f: Failure) {
        if self.room.as_ref().is_some_and(|r| r.placed) {
            self.log(format!(
                "ルームの手順は終わらなかった（{f:?}）が、相手のコードはもう読んでセッションの設定を置いた（繋がるかは P2P の側に任せる）"
            ));
            return;
        }
        // ランダム対戦は、割り当てのルームで相手が揃わなかった（期限切れ・相手が抜けた）ら待ち列へ入り直す。
        if self.random_order.is_some() {
            let since = self.last_requeue.map(|t| t.elapsed().as_secs());
            if should_requeue(f, false, since) {
                return self.random_requeue("割り当てのルームで相手が揃わなかった");
            }
        // 始める側で相手がまだ来ていないルームの期限切れは、同じルーム名で自動で入り直す。それ以外の失敗は、下で知らせて終える
        // （自動で繰り返さない）。
        } else if let Some(r) = self.room.as_ref() {
            let s = Situation {
                first_in: r.side == Some(CodeSide::Offer),
                peer_came: r.peer_came,
                placed: r.placed,
                since_last_rejoin_secs: self.last_rejoin.map(|t| t.elapsed().as_secs()),
            };
            if should_rejoin(f, &s) {
                return self.room_rejoin();
            }
        }
        if let Failure::RateLimited { retry_after_secs } = f {
            self.note.retry_secs = u16::try_from(retry_after_secs).unwrap_or(u16::MAX);
        }
        self.log_problem(format!("ルームで待てなかった: {f:?}"));
        self.room_end(room_failure_event(f));
    }

    fn check_shape_late(&mut self) {
        let Some(placed) = self.placed_shape else {
            return;
        };
        let now = (self.ui.link.shape(), self.ui.link.start_delay());
        if now != placed {
            self.placed_shape = Some(now);
            self.log(
                "対戦の形を、セッションの設定を置いた後に変えられた（この接続では変わらない）",
            );
            self.event(Event::ShapeLate);
        }
    }

    /// 使えるコードが `window` の間に届かなかったら、待ちをやめる（ネットワークは終えない。メニューからやり直せる）。
    fn check_window(&mut self) {
        // ルームで待つ間はルームの期限（10 分。サーバーが 4408 で切り、core が予備の期限を持つ）に任せる。
        if self.room.is_some() || self.queue.is_some() {
            return;
        }
        let Some(t) = self.started else {
            return;
        };
        if self.slot.generation() != 0 || t.elapsed() < self.timing.window {
            return;
        }
        let secs = self.timing.window.as_secs();
        self.sw = None;
        self.accepted = None;
        self.started = None;
        self.placed_shape = None;
        self.note.state = CodeState::Idle;
        self.say(
            &Say::new(
                format!("{secs} 秒の間に使えるコードを読めなかったので、待ちをやめた（ネットワークは残す）"),
                "",
            )
            .kind(Kind::GaveUp),
        );
        self.event(Event::GaveUp);
    }

    fn run(&mut self) {
        loop {
            if self.shared.stop.load(std::sync::atomic::Ordering::Relaxed) {
                self.log("見張りを止めた");
                return;
            }
            if self.slot.is_connected() {
                let shape = self.slot.shape();
                self.say(&connected_say(self.slot.seat(), shape));
                self.log(decided_shape_log(shape));
                // 観戦の配る側は、知らせ（音は鳴り終わるまで待つ）より先に起こす（観戦者を待たせない）。
                self.start_casting();
                // 本ログへ知らせの結果を残す（音は鳴り終わるまで待つが、待つのはこのスレッドだけ）。
                let n = (self.notify)();
                self.log(n);
                return;
            }
            if self.slot.is_done() {
                self.say(
                    &Say::new("ネットワークが終わった（繋がらずに終えた）", "")
                        .kind(Kind::EndedUnconnected),
                );
                self.event(Event::Ended);
                return;
            }
            while let Some(o) = self.ui.link.take_order() {
                self.handle(o);
            }
            // ルームの運び（受信は別のスレッドが積む。ここは取り出して手順を進めるだけで、止まらない）。
            self.queue_tick();
            self.room_tick();
            self.check_shape_late();
            self.check_window();
            std::thread::sleep(GAME_POLL);
        }
    }
}

/// 前後の空白（改行）と BOM を除いた文字（同じコードを、改行の有無や BOM の違いで別のコードと取り違えない）。
fn bare(text: &str) -> &str {
    text.trim_start_matches('\u{FEFF}').trim()
}

fn side_text(side: CodeSide) -> &'static str {
    match side {
        CodeSide::Offer => "ルームに先に入った始める側",
        CodeSide::Answer => "ルームに後から入った返す側",
        CodeSide::Spectate | CodeSide::Game => "側が分からない",
    }
}

/// ルームの手順の失敗を、メニューの出来事に。
fn room_failure_event(f: Failure) -> Event {
    match f {
        Failure::Unresponsive(_) => Event::RoomUnresponsive,
        Failure::RateLimited { .. } => Event::RoomBusy,
        Failure::RoomFull => Event::RoomFull,
        Failure::BadRoomName => Event::RoomRefused,
        Failure::Expired => Event::RoomExpired,
        Failure::PeerLeft => Event::RoomPeerLeft,
    }
}

/// ルームで受けた相手のコードの断りを、メニューの出来事に。バージョン違い・範囲の外・作りの誤りは今までの文、ほか（文字の誤り・期限切れ・
/// 取り違え）は「ルームの相手から届いたコードを読めませんでした」（人がコピーしたものではないので「写し直して」とは言わない）。
fn room_refusal_event(kind: Kind) -> Event {
    match kind {
        Kind::Refused(Refusal::OtherVersion { .. }) => Event::OtherVersion,
        Kind::Refused(Refusal::OtherBuild { .. }) => Event::OtherBuild,
        Kind::Refused(Refusal::NoRoute) => Event::NoRoute,
        Kind::Refused(Refusal::Internal) => Event::Internal,
        _ => Event::RoomBadCode,
    }
}

fn event_of(kind: Kind) -> Event {
    match kind {
        Kind::Refused(r) => match r {
            Refusal::Unreadable => Event::Unreadable,
            Refusal::OwnCode => Event::OwnCode,
            Refusal::BothOffer => Event::BothOffer,
            Refusal::BothAnswer => Event::BothAnswer,
            Refusal::Broken => Event::Broken,
            Refusal::Expired => Event::Expired,
            Refusal::WrongSession => Event::WrongSession,
            Refusal::WrongKind => Event::WrongKind,
            Refusal::NoRoute => Event::NoRoute,
            Refusal::Internal => Event::Internal,
            Refusal::OtherVersion { .. } => Event::OtherVersion,
            Refusal::OtherBuild { .. } => Event::OtherBuild,
        },
        _ => Event::Broken,
    }
}

/// 見張りを起こす（開くスレッドから。`script_win::open_game_net`）: 自分の候補（受信ソケットと、ini の `stun` があれば見えたアドレス）を
/// 決め、見張りのスレッドを起こす。戻りの文は本ログの「ネットワーク: 開いた」の行に足す。
// 検査が個別に差し替える部品を引数で受ける入口。
#[allow(clippy::too_many_arguments)]
pub fn start_game(
    s: &NetSettings,
    net: &mut GameNet,
    role_nonce: u64,
    slot: Arc<LinkSlot>,
    timing: Timing,
    notify: Notify,
    mode: ModeCtx,
    ui: GameUi,
) -> Result<(Watch, String, Option<Parts>), String> {
    let mut note = String::new();
    let local = net.local_addr().ok_or("受信ソケットが分からない")?;
    // UPnP は STUN と並行に走らせ、結果は `nat_settle` で待つ（上限つき。出なければ使えないものとして進む）。
    net_udp::upnp_begin(s, net, &mut note);
    let (mapped, stun) = net_udp::stun_state(s, net, &mut note);
    // IPv6 の経路。開けなければ IPv4 だけで続ける（理由は note に足される）。
    let v6 = net_udp::open_ipv6(s, net, &mut note);
    let nat = net_udp::nat_settle(s, net, mapped, &mut note);
    let own_cands = s
        .candidates_for_upnp(local, mapped, v6, nat.upnp_port)
        .map_err(|e| format!("{e}{note}"))?;
    net.note_own_candidates(own_cands.as_slice());
    // 観戦の配る側（`[spectate] enable = 1`）。観戦用のコードの候補は自分の受信ソケットと STUN で見えたアドレス。
    // 用意できなくても対戦は開く（理由は note に足す）。
    let (armed, spectate_note) = spectate_offer::arm(s, local, mapped, ui.room.clone());
    note.push_str(&spectate_note);
    let (kit, parts) = match armed {
        Some(a) => (Some(a.kit), Some(a.parts)),
        None => (None, None),
    };
    let shared = Arc::new(Shared::new(CodeSide::Game, stun));
    let mut w = GameWatcher::new(
        s,
        role_nonce,
        own_cands,
        slot,
        Arc::clone(&shared),
        timing,
        notify,
        mode,
        ui,
    );
    w.set_nat(nat.mark);
    if let Some(k) = kit {
        w.set_spectate(k);
    }
    w.log(if w.ui.room.is_some() {
        "ゲーム内のメニューの見張りを起こした（対戦のルームマッチ・作る・貼るの指示を待つ。ファイルは使わない）"
    } else {
        "ゲーム内のメニューの見張りを起こした（作る・貼るの指示を待つ。ファイルは使わない）"
    });
    let join = std::thread::Builder::new()
        .name("higanr-code".into())
        .spawn(move || {
            // パニックは受けて外へ出さない。受けたら見張りをやめる（ネットワークは相手の沈黙か時間切れで終わる）。
            let _ = catch_unwind(AssertUnwindSafe(|| w.run()));
        })
        .map_err(|e| format!("接続コードの見張りのスレッドを起こせない: {e}"))?;
    Ok((Watch::from_parts(shared, join), note, parts))
}

// ---- ネットワークの組の開け閉め（つなぎ直し）----
//
// 1 回のつながりに 1 組: 受信ソケットと受信スレッド（`net_udp`）・見張りのスレッドとつながりのセッションの受け渡し（`LinkSlot`）・ネットワークのセッション
// （`NetPlay`）。つながりが終わってマッチの外へ戻ったら、ゲームのスレッドはネットワークのセッションを手放すだけで、重い仕事（スレッドを止めて待つ・
// 受信ソケットを閉じる）はワーカースレッドが `close_parts` で行う。次の「作る」「貼る」対戦の「ルームマッチ」で `open_set` が新しい組を作る。
// 受信ソケットも作り直す（使い回さない）: 自分のコードは開いた後の受信ソケットと STUN の答えから作るので、ポートを取り直してもコードと
// 受信ソケットは食い違わない。使い回すと、前の相手の送り直し・受信スレッドの PONG の口・リングの数が次のつながりへ持ち越される。
// 秘密・session は「作る」のたびに、役割の乱数（nonce）は開くたびに引き直す。前のつながりのコードは貼られても読まない
// （`GameUi::spent`）。

/// 開いたネットワークの組（`open_set` の戻り）。ネットワークのセッション（`np`・`net`）はゲームのスレッドへ渡し、見張りと受信スレッドの持ち手はワーカーが持つ。
pub struct Opened {
    pub np: NetPlay,
    pub net: GameNet,
    pub watch: Watch,
    pub recv: RecvThread,
    /// 開いた受信ソケット（分からなければ None）と、高分解能の待機タイマーで待てるか（本ログ）。
    pub local: Option<std::net::SocketAddr>,
    pub timer: bool,
    /// 本ログの「ネットワーク: 開いた」の行に足す文（受信ソケットの決め方・STUN・見張り）。
    pub note: String,
}

/// ネットワークの組を開く（開くスレッドかワーカースレッド。ゲームのスレッドでは呼ばない —— 受信ソケット・STUN の問い合わせは待つ）。`cfg` はマッチのパスの
/// 設定（`script_win::play_config`）、`nonce` は役割を決める乱数。`ui` のクリップボード・ルームの設定・前のつながりの session は
/// 呼び手が決める（製品は本物のクリップボード、検査は偽物）。
#[allow(clippy::too_many_arguments)] // `start_game` と同じ材料を 1 か所で受ける（検査が部品を差し替える入口）
pub fn open_set(
    s: &NetSettings,
    cfg: crate::netplay::Config,
    nonce: u64,
    timing: Timing,
    notify: Notify,
    mode: ModeCtx,
    ui: GameUi,
) -> Result<Opened, String> {
    let (bind, bind_note) = net_udp::resolve_bind(s)?;
    let (mut net, recv) = net_udp::open_for_code_scoped(s.scope, bind)?;
    let local = net.local_addr();
    let timer = net.high_resolution_wait();
    let slot = Arc::new(LinkSlot::new());
    // 見張りを起こせなければ、ここで `net` と `recv` が落ちて受信ソケットが閉じる（次の「作る」でやり直せる）。
    let (watch, mut note, parts) = start_game(
        s,
        &mut net,
        nonce,
        Arc::clone(&slot),
        timing,
        notify,
        mode,
        ui,
    )?;
    let mut np = NetPlay::with_code_slot(slot, cfg, &mut net);
    // 観戦のリングバッファと送り手（開くたびに新しい組）。送り手を受信スレッドが取れたときだけリングバッファをセッションに置く（読み手のいないリングバッファへ書かない）。
    // リングバッファはマッチのホストのときだけ書く（`NetPlay::set_spectate_for_host`）。
    if let Some(p) = parts {
        if recv.install_spectate(p.sender) {
            np.set_spectate_for_host(p.ring);
        } else {
            note.push_str(" / 観戦: 受信スレッドに置けなかった（観戦は使わない）");
        }
    }
    Ok(Opened {
        np,
        net,
        watch,
        recv,
        local,
        timer,
        note: format!("{bind_note}{note}"),
    })
}

/// ネットワークの組を片づける（ワーカースレッド。ゲームのスレッドがネットワークのセッションを手放した後）。順は ネットワークのセッション → 見張り（止めて待つ）→ 受信スレッド
/// （止めて待つ。最後の持ち手が落ちて受信ソケットが閉じる）→ メニューとのインターフェースの前の見張りの残り。見張りの持ち手は止めるだけで残す
/// （文と数は試験のまとめ・本ログが読む）。戻りは受信スレッドの最後の数（本ログ）。
pub fn close_parts(
    np: NetPlay,
    net: GameNet,
    watch: &mut Watch,
    recv: RecvThread,
    link: &UiLink,
    mark: u32,
) -> net_udp::RecvStats {
    drop(np);
    drop(net);
    watch.stop();
    let stats = recv.stop();
    link.reset_for_next_watcher(mark);
    stats
}

#[cfg(test)]
mod tests {
    //! 偽のクリップボードと、スレッドを起こさない `handle` の直の呼び出しで、メニューからの指示への答えを見る。
    //! 接続コード・秘密・名前・アドレスは合成の値だけ。
    use super::*;
    use std::net::SocketAddr;

    use higanr_core::delay_pick::DelayChoice;
    use higanr_core::net_menu::Event;

    use crate::clipboard::fake::FakeClipboard;
    use crate::netplay::FrameRec;

    fn cands(port: u16) -> Candidates {
        let a: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        Candidates::from_addrs(&[a]).unwrap()
    }

    struct Side {
        w: GameWatcher,
        clip: FakeClipboard,
        link: Arc<UiLink>,
        slot: Arc<LinkSlot>,
    }

    fn side(name: &str, port: u16, nonce: u64, timing: Timing) -> Side {
        side_with(name, port, nonce, timing, "", None)
    }

    /// `extra` は ini の `[netplay]` に足す行（`room_server` / `room`）、`room` は見張りがルームへ繋ぐ設定。
    fn side_with(
        name: &str,
        port: u16,
        nonce: u64,
        timing: Timing,
        extra: &str,
        room: Option<RoomOptions>,
    ) -> Side {
        let clip = FakeClipboard::default();
        let link = Arc::new(UiLink::new());
        let slot = Arc::new(LinkSlot::new());
        let s = crate::net_config::from_text(&format!(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\nname = {name}\ndelay = 3\n{extra}"
        ))
        .unwrap()
        .unwrap();
        let shared = Arc::new(Shared::new(
            CodeSide::Game,
            crate::trial_summary::Stun::NotUsed,
        ));
        let w = GameWatcher::new(
            &s,
            nonce,
            cands(port),
            Arc::clone(&slot),
            shared,
            timing,
            quiet_notify,
            ModeCtx::none(),
            GameUi {
                link: Arc::clone(&link),
                clip: Box::new(clip.clone()),
                room,
                room_last_dir: None,
                spent: Vec::new(),
            },
        );
        Side {
            w,
            clip,
            link,
            slot,
        }
    }

    fn quiet_notify() -> String {
        "（検査: 知らせない）".to_string()
    }

    const T: Timing = Timing {
        poll: Duration::from_millis(20),
        window: Duration::from_secs(30),
    };

    fn pair() -> (Side, Side) {
        (
            side("alice", 41_101, 0xF000, T),
            side("bob", 41_102, 0x0F00, T),
        )
    }

    fn last_event(s: &Side) -> Event {
        s.link.note().expect("状態が書かれていない").event
    }

    #[test]
    fn create_writes_a_fresh_initiator_code_to_the_clipboard() {
        let mut a = side("alice", 41_101, 0xF000, T);
        a.w.handle(Order::Create);
        let text = a.clip.text().expect("クリップボードに入っていない");
        let c = code::read_initiator(&text, net_udp::now_unix())
            .expect("始める側のコードとして読めない");
        assert!(c.secret.is_some());
        assert_eq!(a.clip.writes(), vec![text.clone()]);
        let n = a.link.note().unwrap();
        assert_eq!((n.event, n.state), (Event::Made, CodeState::Offered));
        assert_eq!(n.code_len as usize, text.chars().count());
        assert_eq!(n.head, <[u8; 4]>::try_from(&text.as_bytes()[..4]).unwrap());
        let (h, t) = Note::edges_of(&text);
        assert_eq!((n.head, n.tail), (h, t));
        assert!(n.since as u64 + 5 >= net_udp::now_unix());
        a.w.handle(Order::Create);
        let second = a.clip.text().unwrap();
        assert_ne!(second, text);
        let c2 = code::read_initiator(&second, net_udp::now_unix()).unwrap();
        assert_ne!(c2.session_id, c.session_id);
        assert_ne!(c2.secret.map(|s| s.0), c.secret.map(|s| s.0));
        let mut b = side("bob", 41_102, 0x0F00, T);
        b.clip.0.lock().unwrap().fail_write = true;
        b.w.handle(Order::Create);
        assert_eq!(last_event(&b), Event::ClipWrite);
        assert_eq!(b.link.note().unwrap().state, CodeState::Idle);
        assert!(b.clip.writes().is_empty());
    }

    #[test]
    fn a_name_decided_in_the_menu_is_used_by_the_next_codes_and_the_hello() {
        let (mut a, mut b) = pair();
        let now = net_udp::now_unix();
        a.w.handle(Order::Create);
        let first = code::read_initiator(&a.clip.text().unwrap(), now).unwrap();
        assert_eq!(first.name.as_str(), "alice");
        a.link.send(higanr_core::net_menu::Cmd::Name(
            higanr_core::player_name::NameBuf::from_name("alice_2").unwrap(),
        ));
        b.link.send(higanr_core::net_menu::Cmd::Name(
            higanr_core::player_name::NameBuf::from_name("bob-2").unwrap(),
        ));
        a.w.handle(Order::Create);
        let offer = a.clip.text().unwrap();
        assert_eq!(
            code::read_initiator(&offer, now).unwrap().name.as_str(),
            "alice_2",
            "決めた名前が次の接続コードに載らない"
        );
        assert_eq!(a.w.s.handshake_config(1, 1).name.as_str(), "alice_2");
        b.clip.set(&offer);
        b.w.handle(Order::Paste);
        let units = |n: &Note| -> String {
            n.name
                .iter()
                .take_while(|&&u| u != 0)
                .map(|&u| char::from_u32(u32::from(u)).unwrap())
                .collect()
        };
        assert_eq!(units(&b.link.note().unwrap()), "alice_2");
        assert_eq!(b.w.s.handshake_config(1, 1).name.as_str(), "bob-2");
        let reply = b.clip.text().unwrap();
        a.clip.set(&reply);
        a.w.handle(Order::Paste);
        assert_eq!(units(&a.link.note().unwrap()), "bob-2");
        let mut c = side("carol", 41_103, 0x00F0, T);
        c.w.handle(Order::Create);
        assert_eq!(c.w.s.handshake_config(1, 1).name.as_str(), "carol");
    }

    #[test]
    fn the_round_trip_places_matching_links_on_both_sides() {
        let (mut a, mut b) = pair();
        a.w.handle(Order::Create);
        let offer = a.clip.text().unwrap();
        b.clip.set(&format!("\u{FEFF}{offer}\r\n"));
        b.w.handle(Order::Paste);
        let nb = b.link.note().unwrap();
        assert_eq!((nb.event, nb.state), (Event::ReplyMade, CodeState::Replied));
        assert_eq!(nb.paste_len as usize, offer.chars().count());
        assert_eq!(b.slot.generation(), 1, "返す側はセッションの設定を置いた");
        let reply = b.clip.text().unwrap();
        assert_ne!(reply, offer);
        let na0 = a.link.note().unwrap();
        assert_eq!(na0.state, CodeState::Offered);
        let name: String = nb
            .name
            .iter()
            .take_while(|&&u| u != 0)
            .map(|&u| char::from_u32(u32::from(u)).unwrap())
            .collect();
        assert_eq!(name, "alice");
        a.clip.set(&reply);
        a.w.handle(Order::Paste);
        let na = a.link.note().unwrap();
        assert_eq!((na.event, na.state), (Event::PeerRead, CodeState::Linking));
        assert_eq!(a.slot.generation(), 1);
        a.w.handle(Order::Paste);
        assert_eq!(a.slot.generation(), 1);
        assert_eq!(last_event(&a), Event::Same);
        b.clip.set(&offer);
        b.w.handle(Order::Paste);
        assert_eq!(b.slot.generation(), 1);
        assert_eq!(last_event(&b), Event::ReplyMade);
        assert_eq!(b.clip.text().unwrap(), reply, "同じ返すコード");
    }

    #[test]
    fn each_side_reads_the_nat_mark_the_other_wrote_and_cancelling_forgets_it() {
        use higanr_core::nat::{NatMark, Symmetry};
        let (mut a, mut b) = pair();
        let mark_a = NatMark {
            symmetry: Symmetry::Yes,
            upnp: true,
        };
        let mark_b = NatMark {
            symmetry: Symmetry::No,
            upnp: false,
        };
        a.w.set_nat(mark_a);
        b.w.set_nat(mark_b);
        assert_eq!(a.slot.peer_nat(), None, "読む前は無い");
        a.w.handle(Order::Create);
        let offer = a.clip.text().unwrap();
        // 始める側のコードに自分の印が載っている（読み戻して確かめる）。
        let read = higanr_core::code::read_initiator(&offer, net_udp::now_unix()).unwrap();
        assert_eq!(read.nat, mark_a);
        b.clip.set(&offer);
        b.w.handle(Order::Paste);
        assert_eq!(b.slot.peer_nat(), Some(mark_a), "返す側は相手の印を読んだ");
        let reply = b.clip.text().unwrap();
        a.clip.set(&reply);
        a.w.handle(Order::Paste);
        assert_eq!(
            a.slot.peer_nat(),
            Some(mark_b),
            "始める側は返すコードの印を読んだ"
        );
        // 取りやめたり新しく作ったりしたら忘れる（次の相手に前の相手の印で文を選ばない）。
        a.w.handle(Order::Create);
        assert_eq!(
            a.slot.peer_nat(),
            None,
            "作り直したのに前の相手の印が残った"
        );
        b.w.handle(Order::Cancel);
        assert_eq!(
            b.slot.peer_nat(),
            None,
            "取りやめたのに前の相手の印が残った"
        );
    }

    #[test]
    fn short_empty_and_unreadable_clipboards_are_refused_before_reading() {
        let mut a = side("alice", 41_101, 0xF000, T);
        a.w.handle(Order::Paste);
        assert_eq!(last_event(&a), Event::Empty);
        for text in ["0", "delay", &"x".repeat(code::MIN_CODE_CHARS - 1)] {
            a.clip.set(text);
            a.w.handle(Order::Paste);
            let n = a.link.note().unwrap();
            assert_eq!(n.event, Event::Short, "{text:?}");
            assert_eq!(n.paste_len as usize, text.chars().count());
        }
        a.clip.set(&format!("  {}  \r\n", "y".repeat(10)));
        a.w.handle(Order::Paste);
        assert_eq!(a.link.note().unwrap().paste_len, 10);
        a.clip.0.lock().unwrap().fail_read = true;
        a.w.handle(Order::Paste);
        assert_eq!(last_event(&a), Event::Unreadable);
        assert_eq!(a.slot.generation(), 0, "どれでもセッションの設定を置かない");
        assert_eq!(a.link.note().unwrap().state, CodeState::Idle);
    }

    /// バージョン 0 の始める側のコード（`higanr_core::code` の検査のバージョン 0 の golden と同じ文字。アドレスは loopback と文書用の IPv6）。
    const OFFER_V0: &str =
        "AAFEMyIRANJJawAAAAACYWIAAQIDBAUGBwgJCgsMDQ4PAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBsEJ6ZI";

    #[test]
    fn a_code_of_another_version_is_named_with_its_version() {
        let (mut a, mut b) = pair();
        b.clip.set(OFFER_V0);
        b.w.handle(Order::Paste);
        assert_eq!(last_event(&b), Event::OtherVersion);
        assert_eq!(b.link.note().unwrap().peer_ver, 0);
        assert_eq!(b.link.note().unwrap().state, CodeState::Idle);
        assert!(b.w.sw.is_none(), "断られた貼り付けで返す側にならない");
        assert_eq!(b.slot.generation(), 0, "断ったのにセッションの設定を置いた");
        a.w.handle(Order::Create);
        b.clip.set(&a.clip.text().unwrap());
        b.w.handle(Order::Paste);
        assert_eq!(last_event(&b), Event::ReplyMade);
    }

    #[test]
    fn refusals_name_the_reason_and_keep_the_state() {
        let (mut a, mut b) = pair();
        // 本物のコードの真ん中の 1 文字を替えた（コピー違い。検算が合わない）→ Broken。返す側として作りかけたものは捨てる。
        a.w.handle(Order::Create);
        let offer = a.clip.text().unwrap();
        let mut damaged: Vec<char> = offer.chars().collect();
        let mid = damaged.len() / 2;
        damaged[mid] = if damaged[mid] == 'A' { 'B' } else { 'A' };
        b.clip.set(&damaged.iter().collect::<String>());
        b.w.handle(Order::Paste);
        assert_eq!(last_event(&b), Event::Broken);
        assert_eq!(b.link.note().unwrap().state, CodeState::Idle);
        assert!(b.w.sw.is_none(), "断られた貼り付けで返す側にならない");
        b.w.handle(Order::Paste);
        assert_eq!(last_event(&b), Event::Broken);
        a.clip.set(&offer);
        a.w.handle(Order::Paste);
        assert_eq!(last_event(&a), Event::OwnCode);
        assert_eq!(a.link.note().unwrap().state, CodeState::Offered);
        assert!(a.w.sw.is_some());
        b.w.handle(Order::Create);
        let b_offer = b.clip.text().unwrap();
        a.clip.set(&b_offer);
        a.w.handle(Order::Paste);
        assert_eq!(last_event(&a), Event::BothOffer);
        assert_eq!(a.slot.generation(), 0);
        let mut c = side("carol", 41_103, 0x0A00, T);
        let mut d = side("dave", 41_104, 0x00F0, T);
        c.w.handle(Order::Create);
        let c_offer = c.clip.text().unwrap();
        d.clip.set(&c_offer);
        d.w.handle(Order::Paste);
        let d_reply = d.clip.text().unwrap();
        let mut e = side("erin", 41_105, 0x00A0, T);
        e.clip.set(&d_reply);
        e.w.handle(Order::Paste);
        assert_eq!(last_event(&e), Event::BothAnswer);
        assert!(e.w.sw.is_none());
        // 前の回の自分のコードへの返事（別の session）→ WrongSession。
        c.w.handle(Order::Create);
        c.clip.set(&d_reply);
        c.w.handle(Order::Paste);
        assert_eq!(last_event(&c), Event::WrongSession);
        assert_eq!(c.slot.generation(), 0);
    }

    #[test]
    fn cancel_forgets_the_code_and_replaces_a_started_link() {
        let (mut a, mut b) = pair();
        a.w.handle(Order::Create);
        a.w.handle(Order::Cancel);
        assert_eq!(last_event(&a), Event::Cancelled);
        assert_eq!(a.link.note().unwrap().state, CodeState::Idle);
        assert!(a.w.sw.is_none());
        assert_eq!(
            a.slot.generation(),
            0,
            "セッションはまだ作っていないので何も置かない"
        );
        a.w.handle(Order::Create);
        let offer = a.clip.text().unwrap();
        b.clip.set(&offer);
        b.w.handle(Order::Paste);
        let reply = b.clip.text().unwrap();
        a.clip.set(&reply);
        a.w.handle(Order::Paste);
        assert_eq!(a.slot.generation(), 1);
        a.w.handle(Order::Cancel);
        assert_eq!(a.slot.generation(), 2, "繋がらないセッションに置き直した");
        assert_eq!(last_event(&a), Event::Cancelled);
        assert_eq!(a.link.note().unwrap().state, CodeState::Idle);
    }

    #[test]
    // 何が壊れたら落ちるか: メニューの「サイド」を変えていない側が、表示（おまかせ）と違う席（host_seat の既定の 1P）を名乗る。
    fn the_menu_seat_and_the_code_side_reach_the_link_settings() {
        use higanr_core::seat_pick::SeatAsk;
        use higanr_core::wire::{Arrival, SeatWish};
        let (mut a, mut b) = pair();
        a.link.send(higanr_core::net_menu::Cmd::Seat(SeatWish::Any));
        a.w.handle(Order::Create);
        let offer = a.clip.text().unwrap();
        b.clip.set(&offer);
        b.w.handle(Order::Paste);
        let reply = b.clip.text().unwrap();
        a.clip.set(&reply);
        a.w.handle(Order::Paste);
        assert_eq!(
            a.slot.offered_seat(),
            Some(Some(SeatAsk {
                wish: SeatWish::Any,
                arrival: Arrival::First,
            })),
            "作った側は先・メニューの値"
        );
        assert_eq!(
            b.slot.offered_seat(),
            Some(Some(SeatAsk {
                wish: SeatWish::Any,
                arrival: Arrival::Second,
            })),
            "貼った側は後・変えていなければメニューの初めの値（ini に host_seat が無ければおまかせ）"
        );
    }

    #[test]
    // 何が壊れたら落ちるか: ini に host_seat = 2 を書いた側が、メニューの「サイド」を変えていなくても、その席（2P）の希望を名乗らない
    // （おまかせのまま、または既定の 1P を名乗る）。`initial_seat` を常におまかせにすると落ちる。
    fn an_untouched_menu_names_the_host_seat_from_the_ini() {
        use higanr_core::seat_pick::SeatAsk;
        use higanr_core::wire::{Arrival, SeatWish};
        let mut a = side("alice", 41_101, 0xF000, T);
        let mut b = side_with("bob", 41_102, 0x0F00, T, "host_seat = 2\n", None);
        a.w.handle(Order::Create);
        let offer = a.clip.text().unwrap();
        b.clip.set(&offer);
        b.w.handle(Order::Paste);
        let reply = b.clip.text().unwrap();
        a.clip.set(&reply);
        a.w.handle(Order::Paste);
        assert_eq!(
            b.slot.offered_seat(),
            Some(Some(SeatAsk {
                wish: SeatWish::P2,
                arrival: Arrival::Second,
            })),
            "変えていない貼った側は、ini の host_seat（2）の席を名乗る"
        );
    }

    #[test]
    fn a_shape_change_after_placing_is_told_once() {
        let (mut a, mut b) = pair();
        a.w.handle(Order::Create);
        let offer = a.clip.text().unwrap();
        a.link.send(higanr_core::net_menu::Cmd::Shape(
            Pick::Rollback(2),
            DelayChoice::Fixed(2),
        ));
        b.clip.set(&offer);
        b.w.handle(Order::Paste);
        let reply = b.clip.text().unwrap();
        a.clip.set(&reply);
        a.w.handle(Order::Paste);
        a.w.check_shape_late();
        assert_ne!(last_event(&a), Event::ShapeLate, "置いたときの値のまま");
        a.link.send(higanr_core::net_menu::Cmd::Shape(
            Pick::Delay,
            DelayChoice::Fixed(2),
        ));
        a.w.check_shape_late();
        assert_eq!(last_event(&a), Event::ShapeLate);
        let seq = a.link.note().unwrap().seq;
        a.w.check_shape_late();
        assert_eq!(
            a.link.note().unwrap().seq,
            seq,
            "同じ値のままならもう言わない"
        );
        a.w.handle(Order::Cancel);
        a.link.send(higanr_core::net_menu::Cmd::Shape(
            Pick::Rollback(0),
            DelayChoice::Fixed(0),
        ));
        let seq = a.link.note().unwrap().seq;
        a.w.check_shape_late();
        assert_eq!(a.link.note().unwrap().seq, seq);
    }

    #[test]
    fn placing_puts_the_menu_delay_on_the_slot_and_a_late_delay_change_is_told() {
        let (mut a, mut b) = pair();
        a.w.handle(Order::Create);
        let offer = a.clip.text().unwrap();
        a.link.send(higanr_core::net_menu::Cmd::Shape(
            Pick::Rollback(2),
            DelayChoice::Fixed(5),
        ));
        b.clip.set(&offer);
        b.w.handle(Order::Paste);
        let reply = b.clip.text().unwrap();
        a.clip.set(&reply);
        a.w.handle(Order::Paste);
        assert_eq!(a.slot.start(), Some(DelayChoice::Fixed(5)));
        assert_eq!(b.slot.start(), None, "決めていない側に載った");
        a.w.check_shape_late();
        assert_ne!(last_event(&a), Event::ShapeLate, "置いたときの値のまま");
        a.link.send(higanr_core::net_menu::Cmd::Shape(
            Pick::Rollback(2),
            DelayChoice::Auto,
        ));
        a.w.check_shape_late();
        assert_eq!(last_event(&a), Event::ShapeLate);
    }

    #[test]
    fn the_window_ends_the_wait_without_closing_the_net() {
        let t = Timing {
            poll: Duration::from_millis(10),
            window: Duration::from_millis(50),
        };
        let mut a = side("alice", 41_101, 0xF000, t);
        a.w.check_window();
        assert_ne!(
            last_event_or_none(&a),
            Some(Event::GaveUp),
            "まだ何もしていない"
        );
        a.w.handle(Order::Create);
        a.w.check_window();
        assert_ne!(last_event(&a), Event::GaveUp, "期間の内");
        std::thread::sleep(Duration::from_millis(80));
        a.w.check_window();
        assert_eq!(last_event(&a), Event::GaveUp);
        assert_eq!(a.link.note().unwrap().state, CodeState::Idle);
        assert!(a.w.sw.is_none());
        assert!(
            !a.slot.gave_up(),
            "ネットワークは終わらせない（メニューからやり直せる）"
        );
        // セッションを作った後は、期間が切れても待ちをやめない。
        let (mut a, mut b) = (
            side("alice", 41_101, 0xF000, t),
            side("bob", 41_102, 0x0F00, t),
        );
        a.w.handle(Order::Create);
        b.clip.set(&a.clip.text().unwrap());
        b.w.handle(Order::Paste);
        std::thread::sleep(Duration::from_millis(80));
        b.w.check_window();
        assert_ne!(last_event(&b), Event::GaveUp);
        assert_eq!(b.link.note().unwrap().state, CodeState::Replied);
    }

    fn last_event_or_none(s: &Side) -> Option<Event> {
        s.link.note().map(|n| n.event)
    }

    #[test]
    fn every_refusal_maps_to_its_own_event() {
        let all = [
            (Refusal::Unreadable, Event::Unreadable),
            (Refusal::OwnCode, Event::OwnCode),
            (Refusal::BothOffer, Event::BothOffer),
            (Refusal::BothAnswer, Event::BothAnswer),
            (Refusal::Broken, Event::Broken),
            (Refusal::Expired, Event::Expired),
            (Refusal::WrongSession, Event::WrongSession),
            (Refusal::WrongKind, Event::WrongKind),
            (Refusal::NoRoute, Event::NoRoute),
            (Refusal::Internal, Event::Internal),
            (Refusal::OtherVersion { theirs: 0 }, Event::OtherVersion),
        ];
        let mut seen = std::collections::HashSet::new();
        for (r, e) in all {
            assert_eq!(event_of(Kind::Refused(r)), e);
            assert!(seen.insert(e.code()), "同じ出来事に 2 つの断りが対応する");
        }
        assert_eq!(event_of(Kind::Info), Event::Broken);
    }

    // ---- ルームの経路 ----
    //
    // 部品の検査（毎回の回）: ルーム名の断り・失敗のコピー・項目の出し分け。繋ぐ検査は loopback の偽サーバー（`net_room::fake`）と
    // 本物の WinHTTP・実時間で走るので `#[ignore]`（全件の回が `--ignored` で走らせる）。外へは繋がない（`127.0.0.1` と
    // `Proxy::Direct`）。ルーム名は合成の文字だけ。

    use crate::net_room::fake::FakeServer;
    use crate::net_room::{Base, RoomOptions};
    use crate::room_win::{Proxy, Timeouts};
    use higanr_core::net_menu::{Cmd, RoomItem, RoomNameWhy};
    use higanr_core::room::State as RoomState;

    fn room_opts(base: &str) -> RoomOptions {
        RoomOptions {
            base: Base::parse(base).unwrap(),
            proxy: Proxy::Direct,
            timeouts: Timeouts {
                resolve_ms: 2_000,
                connect_ms: 2_000,
                send_ms: 2_000,
                response_ms: 2_000,
                close_wait_ms: 500,
            },
        }
    }

    fn log_of(s: &Side) -> String {
        s.w.shared
            .notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .join("\n")
    }

    /// `room_tick` と `check_window` を（見張りの本体と同じ順で）`stop` が真になるまで回す。上限を越えたら状態を出して落ちる。
    fn pump(sides: &mut [&mut Side], secs: u64, stop: impl Fn(&[&mut Side]) -> bool) {
        let t0 = Instant::now();
        loop {
            for s in sides.iter_mut() {
                s.w.queue_tick();
                s.w.room_tick();
                s.w.check_window();
            }
            if stop(sides) {
                return;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(secs),
                "{secs} 秒で終わらない: {:?}\n{}",
                sides.iter().map(|s| s.link.note()).collect::<Vec<_>>(),
                sides
                    .iter()
                    .map(|s| log_of(s))
                    .collect::<Vec<_>>()
                    .join("\n---\n")
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// メニューの入力で決めたルーム名を見張りへ渡す（`UiLink::send` ＋ 指示の取り出し。本物の経路と同じ）。空の名は決まりの外。
    fn choose(s: &mut Side, name: &str) {
        s.link
            .send(Cmd::Room(RoomBuf::from_name(name).unwrap_or_default()));
        while let Some(o) = s.link.take_order() {
            s.w.handle(o);
        }
    }

    #[test]
    fn room_names_are_refused_before_connecting_and_never_logged() {
        // 繋ぐ先は使われない（断るので）。万一繋いでも loopback の閉じた番号。
        let opts = || Some(room_opts("ws://127.0.0.1:9"));
        let mut a = side_with(
            "alice",
            41_121,
            0xF000,
            T,
            "room_server = http://127.0.0.1:9\n",
            opts(),
        );
        a.clip.set("synth-clip");
        a.w.handle(Order::Room);
        let n = a.link.note().unwrap();
        assert_eq!(
            (n.event, n.room_why),
            (Event::RoomNameBad, RoomNameWhy::Empty)
        );
        assert!(a.w.room.is_none(), "決まっていない名で繋ぎ始めた");
        assert_eq!(a.clip.0.lock().unwrap().reads, 0, "クリップボードを読んだ");
        choose(&mut a, "");
        let n = a.link.note().unwrap();
        assert_eq!(
            (n.event, n.room_why),
            (Event::RoomNameBad, RoomNameWhy::Empty)
        );
        assert!(a.w.room.is_none(), "空の名で繋ぎ始めた");
        a.w.handle(Order::Create);
        choose(&mut a, "");
        assert_eq!(a.link.note().unwrap().state, CodeState::Offered);
        assert!(a.w.sw.is_some());
        assert_eq!(a.slot.generation(), 0);
        let log = log_of(&a);
        assert!(!log.contains("synth"), "本ログにルーム名: {log}");
        assert!(log.contains("ルーム名を断った"), "{log}");
        // ini の `room` は初めの値にすぎず、見張りは読まない。
        let mut b = side_with(
            "bob",
            41_122,
            0x0F00,
            T,
            "room_server = http://127.0.0.1:9\nroom = synth-ini\n",
            opts(),
        );
        b.w.handle(Order::Room);
        let n = b.link.note().unwrap();
        assert_eq!(
            (n.event, n.room_why),
            (Event::RoomNameBad, RoomNameWhy::Empty),
            "ini の room をルーム名に使った"
        );
        assert!(!log_of(&b).contains("synth"));
    }

    #[test]
    fn room_failures_and_items_map_to_their_own_kinds() {
        use higanr_core::room::Why;
        let all = [
            (
                Failure::Unresponsive(Why::NoAnswer),
                Event::RoomUnresponsive,
            ),
            (
                Failure::RateLimited {
                    retry_after_secs: 60,
                },
                Event::RoomBusy,
            ),
            (Failure::RoomFull, Event::RoomFull),
            (Failure::BadRoomName, Event::RoomRefused),
            (Failure::Expired, Event::RoomExpired),
            (Failure::PeerLeft, Event::RoomPeerLeft),
        ];
        let mut seen = std::collections::HashSet::new();
        for (f, e) in all {
            assert_eq!(room_failure_event(f), e, "{f:?}");
            assert!(seen.insert(e.code()), "{f:?}");
        }
        for (k, e) in [
            (
                Kind::Refused(Refusal::OtherVersion { theirs: 0 }),
                Event::OtherVersion,
            ),
            (Kind::Refused(Refusal::NoRoute), Event::NoRoute),
            (Kind::Refused(Refusal::Internal), Event::Internal),
            (Kind::Refused(Refusal::Broken), Event::RoomBadCode),
            (Kind::Refused(Refusal::Expired), Event::RoomBadCode),
            (Kind::Refused(Refusal::BothOffer), Event::RoomBadCode),
        ] {
            assert_eq!(room_refusal_event(k), e, "{k:?}");
        }
        let initial = RoomBuf::from_name("synth-start").unwrap();
        let item = |extra: &str| {
            crate::script_win::room_item(
                &crate::net_config::from_text(&format!(
                    "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\n{extra}"
                ))
                .unwrap()
                .unwrap(),
                initial,
            )
        };
        assert_eq!(item(""), RoomItem::Off, "room_server が無いのに項目を出す");
        assert_eq!(item("room_server =\n"), RoomItem::Off);
        assert_eq!(
            item("room_server = https://r.example.dev\n"),
            RoomItem::On(initial)
        );
        assert_eq!(
            item("room_server = https://r.example.dev\nroom = synth\n"),
            RoomItem::On(initial),
            "ini の room は入力の初めの値で、項目の出し分けや初めの値の計算（`room_input::initial`）には、ここでは関わらない"
        );
    }

    #[test]
    #[ignore = "loopback の偽サーバーと本物の WinHTTP で実時間に回す（1 秒ほど）。全件の回（build.bat）で --ignored で走る"]
    fn two_watchers_meet_in_a_room_without_touching_the_clipboard() {
        let srv = FakeServer::start();
        let base = srv.base();
        // 始める側（先に入る）は期間を短くして、ルームで待つ間に 30 分の諦めが効かないことも見る。
        let short = Timing {
            poll: Duration::from_millis(10),
            window: Duration::from_millis(30),
        };
        let mut a = side_with(
            "alice",
            41_131,
            0xF000,
            short,
            &format!("room_server = {base}\n"),
            Some(room_opts(&base)),
        );
        let mut b = side_with(
            "bob",
            41_132,
            0x0F00,
            T,
            &format!("room_server = {base}\n"),
            Some(room_opts(&base)),
        );
        let last_dir =
            std::env::temp_dir().join(format!("higanr_room_meet_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&last_dir);
        a.w.ui.room_last_dir = Some(last_dir.clone());
        let b_clip = "  unrelated clipboard text\r\n";
        b.clip.set(b_clip);
        choose(&mut a, "Synth-Room-7");
        let n = a.link.note().unwrap();
        assert_eq!(
            (n.event, n.state),
            (Event::RoomJoining, CodeState::RoomJoining)
        );
        pump(&mut [&mut a], 5, |s| {
            s[0].link.note().unwrap().state == CodeState::RoomWaiting
                && srv.texts_matching(|t| t.starts_with(r#"{"t":"offer""#)) == 1
        });
        assert_eq!(
            a.w.room.as_ref().and_then(|r| r.side),
            Some(CodeSide::Offer)
        );
        let n = a.link.note().unwrap();
        assert_eq!(n.event, Event::RoomWaiting);
        let waited = (net_udp::now_unix() as u32).wrapping_sub(n.since);
        assert!(waited <= 10, "待ち始めた時刻が今でない（{waited} 秒前）");
        assert_eq!(
            room_last::read(&last_dir),
            RoomBuf::from_name("Synth-Room-7"),
            "決めた名を覚えていない"
        );
        let shown: String = n
            .room
            .iter()
            .take_while(|&&u| u != 0)
            .map(|&u| char::from_u32(u32::from(u)).unwrap())
            .collect();
        assert_eq!(shown, "Synth-Room-7", "画面のルーム名");
        std::thread::sleep(Duration::from_millis(60));
        a.w.check_window();
        assert_eq!(a.link.note().unwrap().state, CodeState::RoomWaiting);
        // 後から入る側（大小だけ違う名。サーバーが揃えて同じルームにする）。
        choose(&mut b, "synth-room-7");
        // 置いた瞬間の（ルームの状態, 側）。ルームはその後のサーバーの 1000 で終わって手放すので、後からは読めない。
        type AtPlace = Option<(RoomState, Option<CodeSide>)>;
        let mut b_state_at_place: Option<AtPlace> = None;
        let t0 = Instant::now();
        while a.slot.generation() == 0 || b.slot.generation() == 0 {
            a.w.room_tick();
            b.w.room_tick();
            if b_state_at_place.is_none() && b.slot.generation() > 0 {
                b_state_at_place = Some(b.w.room.as_ref().map(|r| (r.conn.state(), r.side)));
            }
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "置かない: {:?} {:?}\n{}\n---\n{}",
                a.link.note(),
                b.link.note(),
                log_of(&a),
                log_of(&b)
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            b_state_at_place,
            Some(Some((RoomState::AwaitClose, Some(CodeSide::Answer)))),
            "返す側がサーバーの 1000 を待ってから置いた（か、後から入った側が返す側でない）"
        );
        assert_eq!((a.slot.generation(), b.slot.generation()), (1, 1));
        for (s, peer) in [(&a, "bob"), (&b, "alice")] {
            let n = s.link.note().unwrap();
            assert_eq!(
                (n.event, n.state),
                (Event::RoomPeer, CodeState::RoomLinking)
            );
            let name: String = n
                .name
                .iter()
                .take_while(|&&u| u != 0)
                .map(|&u| char::from_u32(u32::from(u)).unwrap())
                .collect();
            assert_eq!(name, peer);
        }
        pump(&mut [&mut a, &mut b], 5, |s| {
            s.iter().all(|x| x.w.room.is_none())
        });
        for s in [&a, &b] {
            assert_eq!(s.link.note().unwrap().event, Event::RoomPeer);
            assert!(log_of(s).contains("ルームの手順を終えた"), "{}", log_of(s));
        }
        assert!(a.clip.writes().is_empty(), "{:?}", a.clip.writes());
        assert!(b.clip.writes().is_empty(), "{:?}", b.clip.writes());
        assert_eq!(b.clip.text().as_deref(), Some(b_clip));
        assert_eq!(
            (
                a.clip.0.lock().unwrap().reads,
                b.clip.0.lock().unwrap().reads
            ),
            (0, 0),
            "ルームの経路がクリップボードを読んだ"
        );
        let _ = std::fs::remove_dir_all(&last_dir);
        // ルーム名は本ログに出ない（パスには打った文字のまま載る）。
        let logs = format!("{}\n{}", log_of(&a), log_of(&b));
        assert!(!logs.to_lowercase().contains("synth-room"), "{logs}");
        assert!(logs.contains("ルームに入った"), "{logs}");
        let paths = srv.seen.paths.lock().unwrap().clone();
        assert!(
            paths.contains(&"/v0/rooms/Synth-Room-7".to_string()),
            "{paths:?}"
        );
        assert!(
            paths.contains(&"/v0/rooms/synth-room-7".to_string()),
            "{paths:?}"
        );
        assert_eq!(srv.texts_matching(|t| t.starts_with(r#"{"t":"answer""#)), 1);
    }

    #[test]
    #[ignore = "loopback の偽サーバーと本物の WinHTTP で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn room_failures_end_the_wait_with_their_own_lines() {
        let srv = FakeServer::start();
        let base = srv.base();
        let watcher = |extra: &str, opts_base: &str| {
            side_with(
                "alice",
                41_141,
                0xF000,
                T,
                &format!("room_server = {base}\n{extra}"),
                Some(room_opts(opts_base)),
            )
        };
        let ends = |s: &mut Side, want: Event| {
            choose(s, "synth-fail");
            pump(&mut [&mut *s], 8, |x| {
                x[0].w.room.is_none() && x[0].link.note().unwrap().event != Event::RoomJoining
            });
            let n = s.link.note().unwrap();
            assert_eq!((n.event, n.state), (want, CodeState::Idle), "{}", log_of(s));
            let mut out = [higanr_core::net_menu::LineBuf::new(); 2];
            let mut st = [higanr_core::ui_layout::RowStyle::Normal; 2];
            let k = higanr_core::net_menu::corner_lines(
                higanr_core::net_menu::Conn::Waiting,
                &n,
                net_udp::now_unix(),
                None,
                higanr_core::lang::Lang::Ja,
                &mut out,
                &mut st,
            );
            assert_eq!(k, 2, "{want:?} の左上の行");
            assert_eq!(s.slot.generation(), 0);
            assert!(s.clip.writes().is_empty());
            assert!(!log_of(s).contains("synth"), "{}", log_of(s));
            (n, out[0].as_str().to_string())
        };
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("ws://{}", l.local_addr().unwrap())
        };
        let mut s = watcher("", &dead);
        let (_, l0) = ends(&mut s, Event::RoomUnresponsive);
        assert_eq!(l0, "サーバーが応答しません。");
        // つなぎ直し: ルームの失敗の後も同じ見張りのまま、接続コードの経路でつなげる。
        s.w.handle(Order::Create);
        let n = s.link.note().unwrap();
        assert_eq!((n.event, n.state), (Event::Made, CodeState::Offered));
        assert!(
            s.clip
                .text()
                .is_some_and(|t| t.len() >= code::MIN_CODE_CHARS)
        );
        for (status, body, want) in [
            (409, r#"{"error":"room_full"}"#, Event::RoomFull),
            (
                429,
                r#"{"error":"rate_limited","retry_after":60}"#,
                Event::RoomBusy,
            ),
            (400, r#"{"error":"bad_room_name"}"#, Event::RoomRefused),
        ] {
            *srv.knobs.forced.lock().unwrap() = Some((status, body.to_string()));
            let mut s = watcher("", &base);
            let (n, _) = ends(&mut s, want);
            if want == Event::RoomBusy {
                assert_eq!(n.retry_secs, 60, "{status}");
            }
        }
        *srv.knobs.forced.lock().unwrap() = None;
        // 4410: 先に入って待つ側。後から入った（生の）返す側が、始める側のコードを受けた後に抜ける。
        let mut s = watcher("", &base);
        choose(&mut s, "synth-fail");
        pump(&mut [&mut s], 5, |_| {
            srv.texts_matching(|t| t.starts_with(r#"{"t":"offer""#)) == 1
        });
        let mut guest =
            RoomConn::start(&room_opts(&base), &RoomName::parse("synth-fail").unwrap()).unwrap();
        let t0 = Instant::now();
        loop {
            if guest
                .poll()
                .iter()
                .any(|e| matches!(e, room::Event::PeerCode(_)))
            {
                break;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "返す側に相手のコードが来ない"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        guest.cancel();
        pump(&mut [&mut s], 5, |x| x[0].w.room.is_none());
        let n = s.link.note().unwrap();
        assert_eq!((n.event, n.state), (Event::RoomPeerLeft, CodeState::Idle));
        assert_eq!(s.slot.generation(), 0);
        drop(guest);
        // 4408（始める側で相手がまだ来ていないルームの期限）: ルームの期限を短くする。1 回目の期限切れは同じルーム名で自動で入り直し
        // （サーバーに届いた offer が 2 通 = 2 回入った）、入り直したルームがすぐ（60 秒より前に）期限切れになった 2 回目は入り直さず、
        // 「期限が切れました」で終える。
        let offers = || srv.texts_matching(|t| t.starts_with(r#"{"t":"offer""#));
        let offers_before = offers();
        srv.knobs
            .room_ttl_ms
            .store(300, std::sync::atomic::Ordering::Release);
        let mut s = watcher("", &base);
        let (_, l0) = ends(&mut s, Event::RoomExpired);
        assert_eq!(l0, "ルームの期限が切れました。");
        assert_eq!(
            offers() - offers_before,
            2,
            "入り直していない（か 2 回以上入り直した）"
        );
        let log = log_of(&s);
        assert_eq!(log.matches("同じルーム名で入り直した").count(), 1, "{log}");
        assert!(!log.contains("synth"), "{log}");
        // 返す側の期限（相手が来た後の期限切れ）は入り直さない: ルームの期限は元へ戻し、相手のコードを返す期限を短くする。始める側が
        // 待っているところへ（生の）返す側が来て、コードを受けたまま返さない。
        srv.knobs
            .room_ttl_ms
            .store(0, std::sync::atomic::Ordering::Release);
        srv.knobs
            .answer_ttl_ms
            .store(400, std::sync::atomic::Ordering::Release);
        let offers_before = offers();
        let mut s = watcher("", &base);
        choose(&mut s, "synth-fail");
        pump(&mut [&mut s], 5, |_| offers() > offers_before);
        let mut guest =
            RoomConn::start(&room_opts(&base), &RoomName::parse("synth-fail").unwrap()).unwrap();
        let t0 = Instant::now();
        let mut got_code = false;
        while !got_code {
            got_code = guest
                .poll()
                .iter()
                .any(|e| matches!(e, room::Event::PeerCode(_)));
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "相手のコードが来ない"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        pump(&mut [&mut s], 8, |x| x[0].w.room.is_none());
        let n = s.link.note().unwrap();
        assert_eq!(
            (n.event, n.state),
            (Event::RoomExpired, CodeState::Idle),
            "{}",
            log_of(&s)
        );
        assert_eq!(offers() - offers_before, 1, "相手が来た後に入り直した");
        assert!(!log_of(&s).contains("入り直した"), "{}", log_of(&s));
        guest.cancel();
    }

    #[test]
    #[ignore = "loopback の偽サーバーと本物の WinHTTP で実時間に回す（1 秒ほど）。全件の回（build.bat）で --ignored で走る"]
    fn choosing_the_same_name_is_ignored_and_another_name_moves_to_the_new_room() {
        let srv = FakeServer::start();
        let base = srv.base();
        let offers = || srv.texts_matching(|t| t.starts_with(r#"{"t":"offer""#));
        let mut a = side_with(
            "alice",
            41_161,
            0xF000,
            T,
            &format!("room_server = {base}\n"),
            Some(room_opts(&base)),
        );
        let last_dir =
            std::env::temp_dir().join(format!("higanr_room_rename_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&last_dir);
        a.w.ui.room_last_dir = Some(last_dir.clone());
        choose(&mut a, "name-x");
        pump(&mut [&mut a], 5, |_| offers() == 1);
        let gen_first = a.w.room_gen;
        choose(&mut a, "name-x");
        assert_eq!(a.w.room_gen, gen_first, "同じ名でルームの経路を作り直した");
        assert!(
            log_of(&a).contains("もう同じルームで待っている"),
            "{}",
            log_of(&a)
        );
        choose(&mut a, "name-y");
        assert_ne!(
            a.w.room_gen, gen_first,
            "別の名なのにルームの経路を作り直さない"
        );
        pump(&mut [&mut a], 5, |_| offers() == 2);
        assert_eq!(
            room_last::read(&last_dir),
            RoomBuf::from_name("name-y"),
            "新しい名を覚えていない"
        );
        let paths = srv.seen.paths.lock().unwrap().clone();
        assert!(paths.contains(&"/v0/rooms/name-x".to_string()), "{paths:?}");
        assert!(paths.contains(&"/v0/rooms/name-y".to_string()), "{paths:?}");
        assert_eq!(a.link.note().unwrap().state, CodeState::RoomWaiting);
        let _ = std::fs::remove_dir_all(&last_dir);
    }

    #[test]
    #[ignore = "loopback の偽サーバーと本物の WinHTTP で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn the_rejoin_keeps_waiting_and_stops_when_the_player_stops_waiting() {
        let srv = FakeServer::start();
        let base = srv.base();
        let offers = || srv.texts_matching(|t| t.starts_with(r#"{"t":"offer""#));
        let mk = |port: u16, nonce: u64| {
            side_with(
                "alice",
                port,
                nonce,
                T,
                &format!("room_server = {base}\n"),
                Some(room_opts(&base)),
            )
        };
        // 入り直したルームは期限の 1.5 秒の間に相手が来られる長さ（ルームの期限はルームを作るときの値）。
        srv.knobs
            .room_ttl_ms
            .store(1_500, std::sync::atomic::Ordering::Release);
        // 待ち続ける間は状態が「待っている」のまま（入り直す瞬間に「つないでいます」へ戻らない）。
        let mut a = mk(41_151, 0xF000);
        choose(&mut a, "synth-rejoin");
        let since = a.link.note().unwrap().since;
        let seen_waiting = std::cell::Cell::new(false);
        let regressed = std::cell::Cell::new(false);
        pump(&mut [&mut a], 5, |x| {
            let n = x[0].link.note().unwrap();
            match n.state {
                CodeState::RoomWaiting => seen_waiting.set(true),
                CodeState::RoomJoining if seen_waiting.get() => regressed.set(true),
                _ => {}
            }
            if seen_waiting.get() && n.event == Event::RoomJoining {
                regressed.set(true);
            }
            log_of(x[0]).contains("入り直した")
                && n.state == CodeState::RoomWaiting
                && offers() >= 2
        });
        let n = a.link.note().unwrap();
        assert_eq!(n.since, since, "入り直して待ち始めた時刻が変わった");
        assert_eq!(n.state, CodeState::RoomWaiting);
        assert!(seen_waiting.get());
        assert!(
            !regressed.get(),
            "入り直す間に状態が「つないでいます」へ戻った"
        );
        srv.knobs
            .room_ttl_ms
            .store(0, std::sync::atomic::Ordering::Release);
        let mut b = mk(41_152, 0x0F00);
        choose(&mut b, "synth-rejoin");
        let t0 = Instant::now();
        while a.slot.generation() == 0 || b.slot.generation() == 0 {
            a.w.room_tick();
            b.w.room_tick();
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "入り直したルームで会えない: {:?} {:?}\n{}\n---\n{}",
                a.link.note(),
                b.link.note(),
                log_of(&a),
                log_of(&b)
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        // 取りやめる・別のつなぎ方を選ぶ: 入り直さない（ルームの状態を手放しているので、期限が来ても何も起きない）。
        for (i, stop) in [Order::Cancel, Order::Create].into_iter().enumerate() {
            let mut c = mk(41_153, 0xF000);
            srv.knobs
                .room_ttl_ms
                .store(300, std::sync::atomic::Ordering::Release);
            let base = offers();
            choose(&mut c, &format!("synth-stop-{i}"));
            // 「待っている」を立てた時点では、始める側のコードはまだ偽サーバーへ送っている途中でありうる。
            // 止める前の 1 通が届いてから数え始める。
            pump(&mut [&mut c], 5, |x| {
                x[0].link.note().unwrap().state == CodeState::RoomWaiting && offers() > base
            });
            let before = offers();
            c.w.handle(stop);
            assert!(c.w.room.is_none(), "{stop:?} の後もルームを持っている");
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(1200) {
                c.w.room_tick();
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(offers(), before, "{stop:?} の後に入り直した");
            assert!(!log_of(&c).contains("入り直した"), "{}", log_of(&c));
        }
    }

    /// 壊れたら落ちる: ルーム・コードの失敗の文が簡潔なログに出なくなる（配線の検査。文の中身はここでは作れない）。
    #[test]
    fn room_and_code_failures_are_pushed_to_the_main_log() {
        let src = include_str!("net_code_game.rs");
        let body = src.split("\n#[cfg(test)]\nmod tests").next().unwrap();
        assert!(
            body.contains("if matches!(s.kind, Kind::Refused(_)) {\n            crate::main_log::push(line.clone());"),
            "断った・受けないの文を本ログへ流していない"
        );
        assert!(
            body.contains("fn log_problem(&self")
                && body.contains(
                    "crate::main_log::push(line.clone());\n        self.shared.note(line);"
                ),
            "log_problem が本ログへ流していない"
        );
        // ルームサーバーへの接続・コードの送受・観戦の配る側・クリップボード・内部の誤りの失敗を通している。
        assert!(body.matches("self.log_problem(").count() >= 14);
    }

    /// `play_two_games` の結果: 両者の本ログ・A と B が名乗った席の希望・進行中のランダム対戦の一覧に載った件。
    type TwoGames = (
        String,
        Option<Option<SeatAsk>>,
        Option<Option<SeatAsk>>,
        Vec<crate::net_room::fake::ListedItem>,
    );

    /// 2 台の見張り（偽サーバー経由）が `cmd_a`・`cmd_b` で相手を見つけてセッションの設定を置いて繋がり、偽のゲームのマッチを 60 フレーム回して、
    /// 両者の列が同じ（同期ズレ 0）なことを見る。A が先に待つ。戻りは両者の本ログの文（検査ごとに名が出ていないかを見る）。
    fn play_two_games(srv: &FakeServer, extra: &str, cmd_a: Cmd, cmd_b: Cmd) -> TwoGames {
        play_two_games_with(srv, extra, extra, cmd_a, cmd_b)
    }

    /// `play_two_games` の、A と B の ini の足しを分けた形。
    fn play_two_games_with(
        srv: &FakeServer,
        extra_a: &str,
        extra_b: &str,
        cmd_a: Cmd,
        cmd_b: Cmd,
    ) -> TwoGames {
        use crate::netplay::fake::{FRAME_US, FakeGame};
        use crate::netplay::{self, CheckSource, NetPlay, Phase, WAIT_BUDGET_US};
        use higanr_core::wire::ByeReason;

        let base = srv.base();
        let mk = |name: &str, extra: &str| {
            crate::net_config::from_text(&format!(
                "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\nname = {name}\ndelay = 3\nroom_server = {base}\n{extra}"
            ))
            .unwrap()
            .unwrap()
        };
        let (sa, sb) = (mk("alice", extra_a), mk("bob", extra_b));
        let (mut na, _ra) = net_udp::open_for_code(sa.bind.fixed().unwrap()).unwrap();
        let (mut nb, _rb) = net_udp::open_for_code(sb.bind.fixed().unwrap()).unwrap();
        let (slot_a, slot_b) = (Arc::new(LinkSlot::new()), Arc::new(LinkSlot::new()));
        let (link_a, link_b) = (Arc::new(UiLink::new()), Arc::new(UiLink::new()));
        let (clip_a, clip_b) = (FakeClipboard::default(), FakeClipboard::default());
        let start = |s: &NetSettings,
                     net: &mut GameNet,
                     nonce,
                     slot: &Arc<LinkSlot>,
                     link: &Arc<UiLink>,
                     clip: &FakeClipboard| {
            start_game(
                s,
                net,
                nonce,
                Arc::clone(slot),
                T,
                quiet_notify,
                ModeCtx::none(),
                GameUi {
                    link: Arc::clone(link),
                    clip: Box::new(clip.clone()),
                    room: Some(room_opts(&base)),
                    room_last_dir: None,
                    spent: Vec::new(),
                },
            )
            .map(|(w, _, parts)| (w, parts))
            .unwrap()
        };
        let (wa, parts_a) = start(&sa, &mut na, 0xF000, &slot_a, &link_a, &clip_a);
        let (wb, parts_b) = start(&sb, &mut nb, 0x0F00, &slot_b, &link_b, &clip_b);
        let expect_listing = extra_a.contains("enable = 1")
            && extra_b.contains("enable = 1")
            && matches!(cmd_a, Cmd::Random);
        let mut listed = Vec::new();
        link_a.send(cmd_a);
        let t0 = Instant::now();
        while link_a.note().map(|n| n.state) != Some(CodeState::RoomWaiting) {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "{:?}\n{:?}",
                link_a.note(),
                wa.notes()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        link_b.send(cmd_b);
        let cfg = netplay::Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            ..netplay::Config::default()
        };
        let mut npa = NetPlay::with_code_slot(Arc::clone(&slot_a), cfg, &mut na);
        let mut npb = NetPlay::with_code_slot(Arc::clone(&slot_b), cfg, &mut nb);
        const FRAMES: u32 = 60;
        let mut ga = FakeGame::new(0xA, FRAMES, 1, 1);
        let mut gb = FakeGame::new(0xB, FRAMES, 1, 1);
        ga.menu_before_decide = 20;
        gb.menu_before_decide = 20;
        let (mut closed_a, mut closed_b) = (false, false);
        let mut connected = false;
        let mut after_end = 0;
        let t0 = Instant::now();
        let mut next = t0;
        loop {
            ga.do_frame(&mut npa, &mut na);
            if ga.done && !closed_a {
                npa.close(ByeReason::Finished);
                closed_a = true;
            }
            gb.do_frame(&mut npb, &mut nb);
            if gb.done && !closed_b {
                npb.close(ByeReason::Finished);
                closed_b = true;
            }
            connected |= npa.connected().is_some() && npb.connected().is_some();
            if listed.is_empty() {
                listed = srv.watch_items(BUILD_NUMBER);
            }
            if npa.phase() == Phase::Ended && npb.phase() == Phase::Ended {
                after_end += 1;
                if after_end > 30 {
                    break;
                }
            }
            assert!(
                t0.elapsed() < Duration::from_secs(30),
                "終わらない: A {:?} {:?} / B {:?} {:?}\nA: {:?}\nB: {:?}",
                npa.phase(),
                npa.end(),
                npb.phase(),
                npb.end(),
                wa.notes(),
                wb.notes()
            );
            next += Duration::from_micros(FRAME_US);
            if let Some(d) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
        }
        // 一覧に載るのはつながった後の見張りの見回り 1 回ぶん遅れる。載るはずの実行は少し待つ。
        let t_list = Instant::now();
        while expect_listing && listed.is_empty() && t_list.elapsed() < Duration::from_secs(3) {
            listed = srv.watch_items(BUILD_NUMBER);
            std::thread::sleep(Duration::from_millis(20));
        }
        // 配る側のスレッドを止める（止めないとテストの後も一覧と観戦用のルームへ入り直し続ける）。
        for p in [&parts_a, &parts_b].into_iter().flatten() {
            p.sender.shared().stop_offer();
        }
        assert!(
            connected,
            "ルームで受けたコードのセッションで繋がらなかった"
        );
        assert_eq!(ga.recs.len(), FRAMES as usize);
        assert_eq!(ga.recs, gb.recs, "両者の試合の列が違う");
        assert_eq!(
            (npa.code_state().links, npb.code_state().links),
            (1, 1),
            "コードから作ったセッションの数"
        );
        assert!(clip_a.writes().is_empty() && clip_b.writes().is_empty());
        let logs = format!("{}\n{}", wa.notes().join("\n"), wb.notes().join("\n"));
        assert!(logs.contains("ルームで相手のコード"), "{logs}");
        assert!(logs.contains("繋がった"), "{logs}");
        // 相手の名前は本ログ向けの文に書かない。
        let lower = logs.to_lowercase();
        assert!(!lower.contains("alice") && !lower.contains("bob"), "{logs}");
        (logs, slot_a.offered_seat(), slot_b.offered_seat(), listed)
    }

    #[test]
    #[ignore = "見張りのスレッド・偽サーバー・本物の UDP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn two_games_meet_in_a_room_and_play_over_loopback() {
        let srv = FakeServer::start();
        let (logs, _, _, listed) = play_two_games(
            &srv,
            "",
            Cmd::Room(RoomBuf::from_name("synth-p2p").unwrap()),
            Cmd::Room(RoomBuf::from_name("SYNTH-P2P").unwrap()),
        );
        assert!(!logs.to_lowercase().contains("synth-p2p"), "{logs}");
        assert!(listed.is_empty(), "ルームマッチが一覧に載った: {listed:?}");
        assert!(
            srv.seen
                .paths
                .lock()
                .unwrap()
                .iter()
                .all(|p| !p.starts_with("/v0/watch/")),
            "ルームマッチが一覧のエンドポイントへ触った"
        );
    }

    #[test]
    #[ignore = "見張りのスレッド・偽サーバー・本物の UDP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn two_games_meet_by_random_match_and_play_over_loopback() {
        use higanr_core::seat_pick::SeatAsk;
        use higanr_core::wire::SeatWish;
        let srv = FakeServer::start();
        // 観戦を有効にすると、ランダム対戦のホストだけが観戦用のルームへ入り、進行中のランダム対戦の一覧に載る。
        let (logs, seat_a, seat_b, listed) =
            play_two_games(&srv, "[spectate]\nenable = 1\n", Cmd::Random, Cmd::Random);
        let assigned = lock_assigned(&srv);
        assert_eq!(assigned.len(), 1, "組は 1 つ: {assigned:?}");
        assert!(
            !logs.to_lowercase().contains(&assigned[0]),
            "割り当てたルーム名が本ログにある: {logs}"
        );
        assert!(
            !logs.to_lowercase().contains(&assigned[0][2..]),
            "割り当てたルーム名の乱数部分が本ログにある: {logs}"
        );
        assert!(logs.contains("ランダム対戦の相手が決まった"), "{logs}");
        assert_eq!(
            logs.matches("進行中のランダム対戦の一覧に載せる").count(),
            1,
            "ホストだけが一覧に載せる: {logs}"
        );
        assert!(
            !logs.contains("観戦の配る側は起こさない（ランダム対戦の相手"),
            "古い断りの文が残っている: {logs}"
        );
        assert!(
            !logs.contains("一覧に載せない設定"),
            "両者が載せてよいのに断った: {logs}"
        );
        let w = RoomName::parse_server_rules(&assigned[0])
            .unwrap()
            .spectate_of();
        assert_eq!(listed.len(), 1, "一覧に載った件: {listed:?}");
        let item = &listed[0];
        assert_eq!(
            item.room,
            w.as_str(),
            "一覧の観戦用のルーム名は割り当ての名から導いたもの"
        );
        let mut names = [item.p1.as_str(), item.p2.as_str()];
        names.sort_unstable();
        assert_eq!(names, ["alice", "bob"], "{item:?}");
        assert_eq!(item.cap, 10);
        assert!(
            ["rollback", "delay"].contains(&item.mode.as_str()),
            "{item:?}"
        );
        assert!(
            logs.contains(&if item.mode == "rollback" {
                format!("対戦の形が決まった: rollback（D {}）", item.delay)
            } else {
                format!(
                    "対戦の形が決まった: delay 方式（遅れ {} フレーム）",
                    item.delay
                )
            }),
            "一覧の同期方式がつながったときに決まった形と違う: {item:?}\n{logs}"
        );
        let paths = srv.seen.paths.lock().unwrap().clone();
        assert!(
            paths
                .iter()
                .any(|p| *p == format!("/v0/rooms/{}", w.as_str())),
            "ホストが観戦用のルームへ入っていない: {paths:?}"
        );
        assert!(
            paths.contains(&format!("/v0/watch/{BUILD_NUMBER}/post")),
            "載せるエンドポイントのパス: {paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|p| *p == format!("/v0/rooms/{}", assigned[0])),
            "割り当てのルームへ入っていない: {paths:?}"
        );
        // 席の希望は同じ（おまかせ）でも、先に待ち列に入った A が先の側として名乗る。
        let ask = |a: Option<Option<SeatAsk>>| a.flatten().map(|s| (s.wish, s.arrival));
        assert_eq!(
            (ask(seat_a), ask(seat_b)),
            (
                Some((SeatWish::Any, higanr_core::wire::Arrival::First)),
                Some((SeatWish::Any, higanr_core::wire::Arrival::Second))
            )
        );
    }

    fn lock_assigned(srv: &FakeServer) -> Vec<String> {
        srv.seen.assigned.lock().unwrap().clone()
    }

    // ---- ランダム対戦（見張りの手順）----

    fn pick_random(s: &mut Side) {
        s.link.send(Cmd::Random);
        while let Some(o) = s.link.take_order() {
            s.w.handle(o);
        }
    }

    fn random_side(name: &str, port: u16, nonce: u64, base: &str, build: u32) -> Side {
        let mut s = side_with(
            name,
            port,
            nonce,
            T,
            &format!("room_server = {base}\n"),
            Some(room_opts(base)),
        );
        s.w.build = build;
        s
    }

    fn state_of(s: &Side) -> CodeState {
        s.link.note().map_or(CodeState::Idle, |n| n.state)
    }

    fn shown_room(n: &Note) -> String {
        n.room
            .iter()
            .take_while(|&&u| u != 0)
            .map(|&u| char::from_u32(u32::from(u)).unwrap_or('?'))
            .collect()
    }

    /// 待ち列に入って「待機中」になるまで回す（偽サーバーが待ち列の人として数えた後）。
    fn queued(srv: &FakeServer, s: &mut Side, upgrades: usize) {
        pump(&mut [s], 5, |x| {
            state_of(x[0]) == CodeState::RoomWaiting
                && srv
                    .seen
                    .queue_upgrades
                    .load(std::sync::atomic::Ordering::Acquire)
                    >= upgrades
        });
    }

    #[test]
    fn a_random_pick_without_a_room_server_is_internal_and_starts_nothing() {
        let mut a = side("alice", 41_141, 0xF000, T);
        pick_random(&mut a);
        let n = a.link.note().unwrap();
        assert_eq!((n.event, n.state), (Event::Internal, CodeState::Idle));
        assert!(a.w.queue.is_none() && a.w.random_order.is_none() && !n.random);
    }

    #[test]
    fn picking_random_names_no_room_and_leaving_it_for_a_named_room_or_cancel_forgets_it() {
        // 繋ぐ先は閉じた loopback の番号（繋ぎ始めの状態だけ見る）。
        let base = "ws://127.0.0.1:9";
        let mut a = random_side("alice", 41_142, 0xF000, base, 7);
        let dir = std::env::temp_dir().join(format!("higanr_random_last_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        a.w.ui.room_last_dir = Some(dir.clone());
        pick_random(&mut a);
        let n = a.link.note().unwrap();
        assert_eq!(
            (n.event, n.state),
            (Event::RoomJoining, CodeState::RoomJoining)
        );
        assert!(n.random, "ランダム対戦の印");
        assert_eq!(shown_room(&n), "", "ルーム名の欄は空");
        assert!(a.w.queue.is_some() && a.w.room.is_none());
        assert_eq!(a.w.random_order, Some(Arrival::Unknown));
        assert_eq!(
            room_last::read(&dir),
            None,
            "ランダム対戦は前に使った名を覚えない"
        );
        // 待っている間にもう一度選ぶ: 読み捨てる（待ち列の順を手放さない）。
        let gen_before = a.w.room_gen;
        pick_random(&mut a);
        assert_eq!(a.w.room_gen, gen_before);
        assert!(log_of(&a).contains("読み捨てた"), "{}", log_of(&a));
        // 名前つきのルームへ移る: 待ち列をやめ、ランダム対戦の印と順を忘れ、名前つきのルーム名を画面に出す。
        choose(&mut a, "named-room");
        let n = a.link.note().unwrap();
        assert!(a.w.queue.is_none() && a.w.room.is_some());
        assert!(!n.random && a.w.random_order.is_none());
        assert_eq!(shown_room(&n), "named-room");
        assert_eq!(room_last::read(&dir), RoomBuf::from_name("named-room"));
        // ランダム対戦へ戻る: 名前つきのルームをやめ、ルーム名の欄を空にする。
        pick_random(&mut a);
        let n = a.link.note().unwrap();
        assert!(a.w.queue.is_some() && a.w.room.is_none());
        assert!(n.random);
        assert_eq!(shown_room(&n), "");
        // 取りやめる。
        a.w.handle(Order::Cancel);
        let n = a.link.note().unwrap();
        assert_eq!((n.event, n.state), (Event::Cancelled, CodeState::Idle));
        assert!(!n.random && a.w.queue.is_none() && a.w.random_order.is_none());
        // 作る・貼るでも経路を忘れる。
        for o in [Order::Create, Order::Paste] {
            pick_random(&mut a);
            a.w.random_met = true;
            a.w.handle(o);
            assert!(a.w.queue.is_none() && a.w.random_order.is_none(), "{o:?}");
            assert!(!a.w.random_met && !a.link.note().unwrap().random, "{o:?}");
        }
        let log = log_of(&a);
        assert!(!log.contains("named-room"), "{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    const ASSIGNED_Q: &str = "q.0123456789abcdef0123456789abcd";

    #[test]
    fn a_random_match_host_starts_the_offer_thread_only_for_an_assigned_room_and_never_as_a_guest()
    {
        use higanr_core::lockstep::Role;
        // 割り当ての形でない名（人が打つ名の経路の残り）: ランダム対戦の印があっても配る側を起こさない。
        let typed = RoomName::parse("synth-meet-room").unwrap();
        let (mut s, a) = spectate_side(0xF000);
        let shared = Arc::clone(a.kit.shared());
        s.w.set_spectate(a.kit);
        s.w.met_room = Some(typed);
        s.w.random_met = true;
        mark(&s, Some(Role::Host));
        s.w.start_casting();
        assert!(
            !shared.get().offer.running,
            "割り当ての形でない名で配る側が起きた"
        );
        let log = log_of(&s);
        assert!(log.contains("割り当てのルーム名を読めない"), "{log}");
        assert!(!log.to_lowercase().contains("synth-meet-room"), "{log}");
        // ホストで割り当てのルーム名なら起きる。ログに名は出ない。
        let q = RoomName::parse_server_rules(ASSIGNED_Q).unwrap();
        let (mut s, a) = spectate_side(0xF000);
        let shared = Arc::clone(a.kit.shared());
        s.w.set_spectate(a.kit);
        s.w.met_room = Some(q);
        s.w.random_met = true;
        mark(&s, Some(Role::Host));
        s.w.start_casting();
        assert!(shared.get().offer.running, "{}", log_of(&s));
        let log = log_of(&s);
        assert!(log.contains("一覧に載せる"), "{log}");
        assert!(!log.contains(&ASSIGNED_Q[2..]), "{log}");
        assert!(!log.contains(q.spectate_of().as_str()), "{log}");
        shared.stop_offer();
        // ゲストは起こさない（ホストだけが配る・載せる）。
        let (mut s, a) = spectate_side(0x0F00);
        let shared = Arc::clone(a.kit.shared());
        s.w.set_spectate(a.kit);
        s.w.met_room = Some(q);
        s.w.random_met = true;
        mark(&s, Some(Role::Guest));
        s.w.start_casting();
        assert!(!shared.get().offer.running, "ゲストが配る側を起こした");
        assert!(log_of(&s).contains("ゲストなので"), "{}", log_of(&s));
        // 観戦を使わない設定（`[spectate]` なし）のホストは起こさず、載せない。
        let mut s = side_with(
            "alice",
            41_132,
            0xF000,
            T,
            "room_server = ws://127.0.0.1:9\n",
            Some(room_opts("ws://127.0.0.1:9")),
        );
        s.w.met_room = Some(q);
        s.w.random_met = true;
        mark(&s, Some(Role::Host));
        s.w.start_casting();
        assert!(!log_of(&s).contains("起こした"), "{}", log_of(&s));
    }

    #[test]
    fn a_random_match_host_neither_lists_nor_casts_when_the_peer_refuses_listing() {
        use higanr_core::lockstep::Role;
        let q = RoomName::parse_server_rules(ASSIGNED_Q).unwrap();
        // ランダムマッチ: 相手が断った（`may_list` が偽）・まだ分からない（印なし）のどちらも起こさない。
        for refused in [true, false] {
            let (mut s, a) = spectate_side(0xF000);
            let shared = Arc::clone(a.kit.shared());
            s.w.set_spectate(a.kit);
            s.w.met_room = Some(q);
            s.w.random_met = true;
            if refused {
                mark_listing(&s, Some(Role::Host), false);
            } else {
                s.slot.mark_role(Role::Host);
                s.slot
                    .mark_connected(higanr_core::wire::Seat::P1, Mode::Delay, 2);
            }
            s.w.start_casting();
            assert!(!shared.get().offer.running, "{}", log_of(&s));
            let log = log_of(&s);
            assert!(log.contains("一覧に載せない設定"), "{log}");
            assert!(!log.contains("起こした"), "{log}");
            assert!(s.w.spectate.is_none(), "材料を使い切っていない");
        }
        // ルームマッチはホストの設定だけで決まる（相手が断っても起こす）。
        let room = RoomName::parse("synth-meet-room").unwrap();
        let (mut s, a) = spectate_side(0xF000);
        let shared = Arc::clone(a.kit.shared());
        s.w.set_spectate(a.kit);
        s.w.met_room = Some(room);
        mark_listing(&s, Some(Role::Host), false);
        s.w.start_casting();
        assert!(shared.get().offer.running, "{}", log_of(&s));
        assert!(!log_of(&s).contains("一覧に載せる"), "{}", log_of(&s));
        shared.stop_offer();
    }

    #[test]
    fn the_listing_names_follow_the_seat_and_carry_the_agreed_shape() {
        use higanr_core::wire::Seat;
        let q = RoomName::parse_server_rules(ASSIGNED_Q).unwrap();
        let peer = higanr_core::handshake::Name::new("zed");
        let mut s = side("alice", 41_133, 0xF000, T);
        s.w.peer_name = Some(peer);
        // 自分が 1P: 自分・相手の順。
        s.slot.mark_connected(Seat::P1, Mode::Rollback, 2);
        let l = s.w.listing_info(&q).unwrap();
        let (p1, p2) = (l.posting.p1.as_str(), l.posting.p2.as_str());
        assert_eq!((p1, p2), ("alice", "zed"));
        assert_eq!(
            (l.posting.mode, l.posting.delay, l.posting.cap),
            (Mode::Rollback, 2, 10)
        );
        assert_eq!(l.build, BUILD_NUMBER);
        // 自分が 2P: 相手・自分の順（席の順に並べる）。
        let s = side("alice", 41_134, 0xF000, T);
        let mut s = s;
        s.w.peer_name = Some(peer);
        s.slot.mark_connected(Seat::P2, Mode::Delay, 5);
        let l = s.w.listing_info(&q).unwrap();
        assert_eq!(
            (l.posting.p1.as_str(), l.posting.p2.as_str()),
            ("zed", "alice")
        );
        assert_eq!((l.posting.mode, l.posting.delay), (Mode::Delay, 5));
        // 載せられない文字の名前は ? に置き換わり、割り当ての形でない名は材料にならない。
        let mut s = side("alice", 41_135, 0xF000, T);
        s.w.s.name = "アリス".to_string();
        s.slot.mark_connected(Seat::P1, Mode::Delay, 0);
        let l = s.w.listing_info(&q).unwrap();
        assert_eq!(l.posting.p1.as_str(), "???");
        assert_eq!(l.posting.p2.as_str(), "?", "相手の名前が分からなければ ?");
        assert!(
            s.w.listing_info(&RoomName::parse("synth-room").unwrap())
                .is_none()
        );
    }

    #[test]
    #[ignore = "見張りのスレッド・偽サーバーと本物の WinHTTP で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn the_first_into_the_queue_is_named_first_even_when_it_reaches_the_room_second() {
        use higanr_core::wire::SeatWish;
        let srv = FakeServer::start();
        let base = srv.base();
        let mut a = random_side("alice", 41_151, 0xF000, &base, 7);
        let mut b = random_side("bob", 41_152, 0x0F00, &base, 7);
        let dir = std::env::temp_dir().join(format!("higanr_random_seats_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        a.w.ui.room_last_dir = Some(dir.clone());
        b.w.ui.room_last_dir = Some(dir.clone());
        pick_random(&mut a);
        queued(&srv, &mut a, 1);
        assert!(lock_assigned(&srv).is_empty(), "1 人では組にならない");
        pick_random(&mut b);
        // B だけを回して、先にルームへ着かせる（A は待ち列の組の知らせを受けても、まだ動かさない）。
        pump(&mut [&mut b], 5, |x| {
            x[0].w
                .room
                .as_ref()
                .is_some_and(|r| r.side == Some(CodeSide::Offer))
        });
        pump(&mut [&mut a, &mut b], 5, |x| {
            x.iter().all(|s| state_of(s) == CodeState::RoomLinking)
        });
        assert!(
            log_of(&a).contains(side_text(CodeSide::Answer)),
            "{}",
            log_of(&a)
        );
        assert!(
            log_of(&b).contains(side_text(CodeSide::Offer)),
            "{}",
            log_of(&b)
        );
        let ask = |s: &Side| s.slot.offered_seat().flatten().map(|k| (k.wish, k.arrival));
        assert_eq!(
            ask(&a),
            Some((SeatWish::Any, Arrival::First)),
            "ルームに後から着いても、待ち列に先に入った A が先の側"
        );
        assert_eq!(ask(&b), Some((SeatWish::Any, Arrival::Second)));
        let assigned = lock_assigned(&srv);
        assert_eq!(assigned.len(), 1);
        for s in [&a, &b] {
            let n = s.link.note().unwrap();
            assert!(n.random && shown_room(&n).is_empty(), "{n:?}");
            assert!(s.w.random_met);
            assert_eq!(
                s.w.met_room.as_ref().map(|r| r.as_str()),
                Some(assigned[0].as_str()),
                "観戦の一覧に載せるため、割り当てたルーム名を内側にだけ持つ"
            );
            let log = log_of(s).to_lowercase();
            assert!(
                !log.contains(&assigned[0]),
                "割り当てたルーム名がログにある: {log}"
            );
            assert!(!log.contains(&assigned[0][2..]), "{log}");
        }
        assert_eq!(room_last::read(&dir), None, "割り当てたルーム名を覚えた");
        assert!(!dir.exists(), "ランダム対戦が覚えるファイルを作った");
        let paths = srv.seen.paths.lock().unwrap().clone();
        assert_eq!(
            paths.iter().filter(|p| *p == "/v0/random/7").count(),
            2,
            "待ち列のパスにビルド番号: {paths:?}"
        );
    }

    #[test]
    #[ignore = "見張りのスレッド・偽サーバーと本物の WinHTTP で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn watchers_with_different_build_numbers_never_pair() {
        let srv = FakeServer::start();
        let base = srv.base();
        let mut a = random_side("alice", 41_153, 0xF000, &base, 7);
        let mut b = random_side("bob", 41_154, 0x0F00, &base, 8);
        pick_random(&mut a);
        queued(&srv, &mut a, 1);
        pick_random(&mut b);
        queued(&srv, &mut b, 2);
        std::thread::sleep(Duration::from_millis(300));
        pump(&mut [&mut a, &mut b], 1, |_| true);
        assert!(
            lock_assigned(&srv).is_empty(),
            "ビルド番号が違うのに組にした"
        );
        assert_eq!(state_of(&a), CodeState::RoomWaiting);
        assert_eq!(state_of(&b), CodeState::RoomWaiting);
        let paths = srv.seen.paths.lock().unwrap().clone();
        assert!(paths.contains(&"/v0/random/7".to_owned()), "{paths:?}");
        assert!(paths.contains(&"/v0/random/8".to_owned()), "{paths:?}");
        // 同じビルド番号の人が来れば、その人と組になる。
        let mut c = random_side("carol", 41_155, 0x00F0, &base, 7);
        pick_random(&mut c);
        pump(&mut [&mut a, &mut b, &mut c], 5, |x| {
            state_of(x[0]) == CodeState::RoomLinking || x[0].w.room.is_some()
        });
        assert_eq!(lock_assigned(&srv).len(), 1);
        assert!(
            b.w.queue.is_some() && b.w.room.is_none(),
            "別のビルドの B は待ち続ける"
        );
    }

    #[test]
    #[ignore = "見張りのスレッド・偽サーバーと本物の WinHTTP で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn three_and_four_watchers_pair_two_by_two_and_a_cancel_leaves_the_queue() {
        let srv = FakeServer::start();
        let base = srv.base();
        let mut a = random_side("alice", 41_156, 0xF000, &base, 7);
        let mut b = random_side("bob", 41_157, 0x0F00, &base, 7);
        let mut c = random_side("carol", 41_158, 0x00F0, &base, 7);
        let mut d = random_side("dave", 41_159, 0x000F, &base, 7);
        pick_random(&mut a);
        queued(&srv, &mut a, 1);
        pick_random(&mut b);
        pump(&mut [&mut a, &mut b], 5, |x| {
            x.iter().all(|s| s.w.room.is_some())
        });
        assert_eq!(lock_assigned(&srv).len(), 1);
        // 3 人目は待つ。
        pick_random(&mut c);
        queued(&srv, &mut c, 3);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(lock_assigned(&srv).len(), 1, "3 人目が組にされた");
        // 3 人目が抜ける: 4 人目は 3 人目と組にならず、待つ。
        c.w.handle(Order::Cancel);
        let n = c.link.note().unwrap();
        assert_eq!((n.event, n.state), (Event::Cancelled, CodeState::Idle));
        let t0 = Instant::now();
        while srv.seen.client_closes.lock().unwrap().is_empty() {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "取りやめがサーバーへ届かない"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        pick_random(&mut d);
        queued(&srv, &mut d, 4);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(lock_assigned(&srv).len(), 1, "抜けた人と組にされた");
        // 5 人目 E と 4 人目 D が組になる。
        let mut e = random_side("erin", 41_160, 0x00A0, &base, 7);
        pick_random(&mut e);
        pump(&mut [&mut d, &mut e], 5, |x| {
            x.iter().all(|s| s.w.room.is_some())
        });
        let names = lock_assigned(&srv);
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1], "組ごとに別のルーム名");
    }

    #[test]
    #[ignore = "見張りのスレッド・偽サーバーと本物の WinHTTP で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn an_expired_queue_entry_requeues_without_resetting_the_clock() {
        let srv = FakeServer::start();
        srv.knobs
            .random_queue_ttl_ms
            .store(300, std::sync::atomic::Ordering::Release);
        let base = srv.base();
        let mut a = random_side("alice", 41_161, 0xF000, &base, 7);
        pick_random(&mut a);
        let since = a.link.note().unwrap().since;
        queued(&srv, &mut a, 1);
        pump(&mut [&mut a], 10, |_| {
            srv.seen
                .queue_upgrades
                .load(std::sync::atomic::Ordering::Acquire)
                >= 2
        });
        assert_eq!(state_of(&a), CodeState::RoomWaiting);
        assert!(log_of(&a).contains("待ち列へ入り直した"), "{}", log_of(&a));
        let n = a.link.note().unwrap();
        assert_eq!(n.since, since, "待った時間の起点が動いた");
        assert!(n.random);
        assert!(a.w.queue.is_some());
    }

    #[test]
    #[ignore = "見張りのスレッド・偽サーバーと本物の WinHTTP で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn a_partner_that_never_enters_the_assigned_room_makes_the_other_requeue() {
        let srv = FakeServer::start();
        srv.knobs
            .random_room_ttl_ms
            .store(300, std::sync::atomic::Ordering::Release);
        let base = srv.base();
        let mut a = random_side("alice", 41_162, 0xF000, &base, 7);
        let mut b = random_side("bob", 41_163, 0x0F00, &base, 7);
        pick_random(&mut a);
        queued(&srv, &mut a, 1);
        pick_random(&mut b);
        // B は動かさない（組の知らせを受けず、ルームへ入らない）。A だけが入ってルームの期限で戻る。
        pump(&mut [&mut a], 10, |_| {
            srv.seen
                .queue_upgrades
                .load(std::sync::atomic::Ordering::Acquire)
                >= 3
        });
        assert!(
            log_of(&a).contains("割り当てのルームで相手が揃わなかった"),
            "{}",
            log_of(&a)
        );
        assert_eq!(state_of(&a), CodeState::RoomWaiting);
        assert!(a.w.queue.is_some() && a.w.room.is_none());
        // 次に来た人と、新しいルームで組になる。
        let mut c = random_side("carol", 41_164, 0x00F0, &base, 7);
        pick_random(&mut c);
        pump(&mut [&mut a, &mut c], 5, |x| {
            x.iter().all(|s| s.w.room.is_some())
        });
        let names = lock_assigned(&srv);
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
    }

    struct Relinker {
        s: NetSettings,
        link: Arc<UiLink>,
        clip: FakeClipboard,
        spent: Vec<u32>,
        set: Option<Opened>,
    }

    impl Relinker {
        fn new(name: &str) -> Relinker {
            Relinker {
                s: relink_settings(name, "127.0.0.1:0"),
                link: Arc::new(UiLink::new()),
                clip: FakeClipboard::default(),
                spent: Vec::new(),
                set: None,
            }
        }

        fn open(&mut self, nonce: u64) {
            let cfg = crate::netplay::Config {
                check: crate::netplay::CheckSource::InputDigest,
                wait_budget_us: crate::netplay::WAIT_BUDGET_US,
                match_gate: true,
                ..crate::netplay::Config::default()
            };
            let o = open_set(
                &self.s,
                cfg,
                nonce,
                T,
                quiet_notify,
                ModeCtx::none(),
                GameUi {
                    link: Arc::clone(&self.link),
                    clip: Box::new(self.clip.clone()),
                    room: None,
                    room_last_dir: None,
                    spent: self.spent.clone(),
                },
            )
            .expect("ネットワークの組を開けない");
            self.set = Some(o);
        }

        fn port(&self) -> u16 {
            self.set.as_ref().and_then(|o| o.local).unwrap().port()
        }

        /// 片づける（`script_win::relink_teardown` → ワーカーの `relink_tick` と同じ順: 使った session を控え、`close_parts`）。
        fn close(&mut self) {
            let Opened {
                np,
                net,
                mut watch,
                recv,
                ..
            } = self.set.take().expect("開いていない");
            if let Some(s) = np.spent_session() {
                self.spent.push(s);
            }
            let mark = self.link.order_mark();
            close_parts(np, net, &mut watch, recv, &self.link, mark);
            assert!(watch.finished(), "見張りを止めていない");
        }

        fn order(&self, c: higanr_core::net_menu::Cmd, want: Event) {
            self.link.send(c);
            let t0 = Instant::now();
            while self.link.note().map(|n| n.event) != Some(want) {
                let notes = self.set.as_ref().map(|o| o.watch.notes());
                assert!(
                    t0.elapsed() < Duration::from_secs(5),
                    "{c:?} の後の出来事が {want:?} にならない: {:?}\n{notes:?}",
                    self.link.note()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn relink_settings(name: &str, bind: &str) -> NetSettings {
        crate::net_config::from_text(&format!(
            "[netplay]\nenable = 1\nbind = {bind}\ncode_side = game\nclipboard = file\nname = {name}\ndelay = 3\n"
        ))
        .unwrap()
        .unwrap()
    }

    /// 1 回のつながり: A が作り、B が貼って返し、A が貼る → 両者が選ぶ（READY）→ 偽のゲームのマッチ `frames` フレーム → B が終えて BYE
    /// → A は相手の BYE で終わる → 両者がマッチの外で片づけてよくなるまで回す。戻りは（A の列、B の列、A のコードの session）。
    fn relink_round(
        a: &mut Relinker,
        b: &mut Relinker,
        seeds: (u64, u64),
        frames: u32,
    ) -> (Vec<FrameRec>, Vec<FrameRec>, u32) {
        use crate::netplay::fake::{FRAME_US, FakeGame};
        use crate::netplay::{End, Game, Net, Phase, RELINK_GRACE_MS};
        use higanr_core::net_menu::Conn;
        use higanr_core::wire::ByeReason;
        a.order(Cmd::Create, Event::Made);
        let offer = a.clip.text().unwrap();
        let session = code::read_initiator(&offer, net_udp::now_unix())
            .unwrap()
            .session_id;
        b.clip.set(&offer);
        b.order(Cmd::Paste, Event::ReplyMade);
        a.clip.set(&b.clip.text().unwrap());
        a.order(Cmd::Paste, Event::PeerRead);
        let mut ga = FakeGame::new(seeds.0, frames, 1, 1);
        let mut gb = FakeGame::new(seeds.1, frames, 1, 1);
        ga.menu_before_decide = 20;
        gb.menu_before_decide = 20;
        let mut closed_b = false;
        let t0 = Instant::now();
        let mut next = t0;
        loop {
            for (side, g) in [(&mut *a, &mut ga), (&mut *b, &mut gb)] {
                let o = side.set.as_mut().unwrap();
                if matches!(o.np.ui_conn(0).0, Conn::Found { ready: false, .. }) {
                    o.np.set_ready(&mut o.net);
                }
                g.do_frame(&mut o.np, &mut o.net);
            }
            if gb.done && !closed_b {
                let o = b.set.as_mut().unwrap();
                o.np.close(ByeReason::Finished);
                closed_b = true;
            }
            let due = |o: &mut Opened, g: &mut FakeGame| {
                let now = (o.net.now_us() / 1_000) as u32;
                o.np.relink_due(now, g.scenes(), RELINK_GRACE_MS)
            };
            if due(a.set.as_mut().unwrap(), &mut ga) && due(b.set.as_mut().unwrap(), &mut gb) {
                break;
            }
            let (pa, pb) = (&a.set.as_ref().unwrap().np, &b.set.as_ref().unwrap().np);
            assert!(
                t0.elapsed() < Duration::from_secs(40),
                "片づけてよくならない: A {:?} {:?} / B {:?} {:?}",
                pa.phase(),
                pa.end(),
                pb.phase(),
                pb.end()
            );
            next += Duration::from_micros(FRAME_US);
            if let Some(d) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
        }
        let (pa, pb) = (&a.set.as_ref().unwrap().np, &b.set.as_ref().unwrap().np);
        assert_eq!(pa.phase(), Phase::Ended);
        assert_eq!(
            pa.end(),
            Some(End::PeerBye(ByeReason::Finished)),
            "A は相手の BYE で終わる"
        );
        assert_eq!(pb.end(), Some(End::Closed(ByeReason::Finished)));
        assert_eq!(
            (pa.code_state().links, pb.code_state().links),
            (1, 1),
            "コードから作ったセッションの数"
        );
        assert_eq!(pa.code_state().session, session);
        (ga.recs, gb.recs, session)
    }

    #[test]
    #[ignore = "見張りのスレッド・本物の UDP（loopback）で実時間に回す（2 回のつながりと片づけの待ちで十数秒）。全件の回（build.bat）で --ignored で走る"]
    fn two_games_play_part_and_connect_again_with_new_codes_over_loopback() {
        let mut a = Relinker::new("alice");
        let mut b = Relinker::new("bob");
        a.open(0xF000);
        b.open(0x0F00);
        let port_a = a.port();
        let first_offer = {
            let (ra, rb, session) = relink_round(&mut a, &mut b, (0xA1, 0xB1), 60);
            assert_eq!(ra.len(), 60, "1 回目の試合が最後まで進まない");
            assert_eq!(ra, rb, "1 回目の両者の列が違う");
            (a.clip.writes()[0].clone(), session)
        };
        a.close();
        b.close();
        assert_eq!(
            a.spent,
            vec![first_offer.1],
            "つながった session を控えていない"
        );
        assert_eq!(b.spent, vec![first_offer.1]);
        for s in [&a, &b] {
            let n = s.link.note().unwrap();
            assert_eq!((n.event, n.state), (Event::None, CodeState::Idle));
        }
        // 2 回目。A は 1 回目と同じポートを取り直す（受信ソケットが閉じていなければ bind に失敗する）。
        a.s = relink_settings("alice", &format!("127.0.0.1:{port_a}"));
        a.open(0xF000);
        b.open(0x0F00);
        assert_eq!(a.port(), port_a);
        // B のクリップボードには前の相手のコードが残っている: 貼っても読まない（前の相手へつなぎに行かない）。
        b.clip.set(&first_offer.0);
        b.order(higanr_core::net_menu::Cmd::Paste, Event::Spent);
        assert_eq!(
            b.set.as_ref().unwrap().np.code_state().links,
            0,
            "前のつながりのコードでセッションの設定を置いた"
        );
        let (ra, rb, session) = relink_round(&mut a, &mut b, (0xA2, 0xB2), 90);
        assert_ne!(
            session, first_offer.1,
            "2 回目のコードが 1 回目の session のまま"
        );
        let second_offer = a.clip.writes().last().cloned().unwrap();
        assert_ne!(second_offer, first_offer.0, "2 回目のコードが 1 回目と同じ");
        assert_eq!(ra.len(), 90, "2 回目の試合が最後まで進まない");
        assert_eq!(ra, rb, "2 回目の両者の列が違う");
        a.close();
        b.close();
    }

    // ---- 観戦の配る側 ----

    use crate::spectate_offer::{Armed, arm_with};
    use higanr_core::net_menu::sanitize_room;

    fn spectate_side(nonce: u64) -> (Side, Armed) {
        let s = side_with(
            "alice",
            41_131,
            nonce,
            T,
            "room_server = ws://127.0.0.1:9\n[spectate]\nenable = 1\n",
            Some(room_opts("ws://127.0.0.1:9")),
        );
        let a = arm_with(
            &s.w.s,
            "127.0.0.1:41131".parse().unwrap(),
            None,
            Some(room_opts("ws://127.0.0.1:9")),
            || Ok(0x5EED_0C01),
            || Ok(Secret([0x31; 16])),
        )
        .unwrap();
        (s, a)
    }

    /// 繋がった印を置く（両者が一覧に載せてよいと名乗った形）。
    fn mark(s: &Side, role: Option<higanr_core::lockstep::Role>) {
        mark_listing(s, role, true);
    }

    fn mark_listing(s: &Side, role: Option<higanr_core::lockstep::Role>, may_list: bool) {
        use higanr_core::wire::{Mode, Seat};
        if let Some(r) = role {
            s.slot.mark_role(r);
        }
        s.slot.mark_may_list(may_list);
        s.slot.mark_connected(Seat::P1, Mode::Delay, 2);
    }

    #[test]
    fn the_offer_thread_starts_only_for_a_host_that_met_in_a_room() {
        use higanr_core::lockstep::Role;
        let room = RoomName::parse("synth-meet-room").unwrap();
        let (mut s, a) = spectate_side(0xF000);
        let shared = Arc::clone(a.kit.shared());
        s.w.met_room = Some(room);
        mark(&s, Some(Role::Host));
        s.w.start_casting();
        assert!(!log_of(&s).contains("観戦"), "{}", log_of(&s));
        s.w.set_spectate(a.kit);
        s.w.met_room = None;
        s.w.start_casting();
        assert!(log_of(&s).contains("会った相手ではない"), "{}", log_of(&s));
        assert!(!shared.get().offer.running);
        let (mut s, a) = spectate_side(0x0F00);
        let shared = Arc::clone(a.kit.shared());
        s.w.set_spectate(a.kit);
        s.w.met_room = Some(room);
        mark(&s, Some(Role::Guest));
        s.w.start_casting();
        assert!(log_of(&s).contains("ゲストなので"), "{}", log_of(&s));
        assert!(!shared.get().offer.running, "ゲストが配る側を起こした");
        let (mut s, a) = spectate_side(0xF000);
        let shared = Arc::clone(a.kit.shared());
        s.w.set_spectate(a.kit);
        s.w.met_room = Some(room);
        mark(&s, None);
        s.w.start_casting();
        assert!(log_of(&s).contains("役が分からない"), "{}", log_of(&s));
        assert!(!shared.get().offer.running);
        let (mut s, a) = spectate_side(0xF000);
        let shared = Arc::clone(a.kit.shared());
        s.w.set_spectate(a.kit);
        s.w.met_room = Some(room);
        mark(&s, Some(Role::Host));
        s.w.start_casting();
        assert!(shared.get().offer.running, "{}", log_of(&s));
        assert!(
            log_of(&s).contains("観戦の配る側を起こした"),
            "{}",
            log_of(&s)
        );
        assert!(
            !log_of(&s).to_lowercase().contains("synth-meet-room"),
            "{}",
            log_of(&s)
        );
        shared.stop_offer();
        assert!(!shared.get().offer.running);
        s.w.start_casting();
        for o in [Order::Create, Order::Paste, Order::Cancel] {
            let (mut s, _a) = spectate_side(0xF000);
            s.w.met_room = Some(room);
            s.w.handle(o);
            assert!(
                s.w.met_room.is_none(),
                "{o:?} の後もルームで会った記憶が残る"
            );
        }
    }

    #[test]
    #[ignore = "見張りのスレッド・偽サーバー・本物の UDP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn the_room_host_hands_a_spectator_a_punch_through_the_spectate_room() {
        use crate::netplay::fake::{FRAME_US, FakeGame};
        use crate::netplay::{self, CheckSource, Phase, WAIT_BUDGET_US};
        use higanr_core::net_menu::Cmd;
        use higanr_core::spectate::PUNCH_PACKET;
        use higanr_core::wire::ByeReason;

        let srv = FakeServer::start();
        let base = srv.base();
        let mk = |name: &str| {
            crate::net_config::from_text(&format!(
                "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\nname = {name}\ndelay = 3\nroom_server = {base}\n[spectate]\nenable = 1\n"
            ))
            .unwrap()
            .unwrap()
        };
        let (sa, sb) = (mk("alice"), mk("bob"));
        let cfg = netplay::Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            ..netplay::Config::default()
        };
        let (link_a, link_b) = (Arc::new(UiLink::new()), Arc::new(UiLink::new()));
        let open = |s: &NetSettings, nonce: u64, link: &Arc<UiLink>| {
            open_set(
                s,
                cfg,
                nonce,
                T,
                quiet_notify,
                ModeCtx::none(),
                GameUi {
                    link: Arc::clone(link),
                    clip: Box::new(FakeClipboard::default()),
                    room: Some(room_opts(&base)),
                    room_last_dir: None,
                    spent: Vec::new(),
                },
            )
            .expect("ネットワークの組を開けない")
        };
        let mut oa = open(&sa, 0xF000, &link_a);
        let mut ob = open(&sb, 0x0F00, &link_b);
        assert!(oa.note.contains("観戦の配る側を用意した"), "{}", oa.note);
        let shared_a = oa.recv.spectate_shared().expect("ホストの観戦の共有");
        let shared_b = ob.recv.spectate_shared().expect("ゲストの観戦の共有");
        let wait = |what: &str, mut cond: Box<dyn FnMut() -> bool>| {
            let t0 = Instant::now();
            while !cond() {
                assert!(t0.elapsed() < Duration::from_secs(5), "{what} にならない");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        link_a.send(Cmd::Room(RoomBuf::from_name("synth-watch-room").unwrap()));
        {
            let l = Arc::clone(&link_a);
            wait(
                "A が対戦のルームマッチで待つ",
                Box::new(move || l.note().map(|n| n.state) == Some(CodeState::RoomWaiting)),
            );
        }
        link_b.send(Cmd::Room(RoomBuf::from_name("SYNTH-WATCH-ROOM").unwrap()));
        const FRAMES: u32 = 420;
        let mut ga = FakeGame::new(0xA, FRAMES, 1, 1);
        let mut gb = FakeGame::new(0xB, FRAMES, 1, 1);
        ga.menu_before_decide = 20;
        gb.menu_before_decide = 20;
        // 観戦者（偽）: 配る側が観戦用のコードを預けたら観戦用のルームへ入り、返しを出す。穴あけの印を受ける受信ソケット。
        let spec_room = RoomName::parse("synth-watch-room").unwrap().spectate_of();
        let viewer_sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        viewer_sock.set_nonblocking(true).unwrap();
        let viewer_addr = viewer_sock.local_addr().unwrap();
        let mut viewer: Option<RoomConn> = None;
        let mut replied = false;
        let mut punches = 0u32;
        let (mut closed_a, mut closed_b) = (false, false);
        let mut after_end = 0;
        let t0 = Instant::now();
        let mut next = t0;
        loop {
            ga.do_frame(&mut oa.np, &mut oa.net);
            if ga.done && !closed_a {
                oa.np.close(ByeReason::Finished);
                closed_a = true;
            }
            gb.do_frame(&mut ob.np, &mut ob.net);
            if gb.done && !closed_b {
                ob.np.close(ByeReason::Finished);
                closed_b = true;
            }
            if viewer.is_none() && shared_a.get().offer.offered >= 1 {
                viewer = Some(RoomConn::start(&room_opts(&base), &spec_room).unwrap());
            }
            if let Some(c) = viewer.as_mut() {
                for e in c.poll() {
                    if let higanr_core::room::Event::PeerCode(text) = e {
                        let spec = code::read_spectate(&text, net_udp::now_unix()).unwrap();
                        let reply = code::write_spectate_reply_to(
                            &spec.code,
                            higanr_core::handshake::Name::new("viewer"),
                            Candidates::from_addrs(&[viewer_addr]).unwrap(),
                            net_udp::now_unix(),
                        )
                        .unwrap();
                        c.submit_code(&reply).unwrap();
                        replied = true;
                    }
                }
            }
            let mut buf = [0u8; 64];
            while let Ok((n, _)) = viewer_sock.recv_from(&mut buf) {
                assert_eq!(
                    &buf[..n],
                    &PUNCH_PACKET,
                    "観戦者の受信ソケットに穴あけの印以外が来た"
                );
                punches += 1;
            }
            if oa.np.phase() == Phase::Ended && ob.np.phase() == Phase::Ended {
                after_end += 1;
                if after_end > 30 {
                    break;
                }
            }
            assert!(
                t0.elapsed() < Duration::from_secs(30),
                "終わらない: A {:?} {:?} / B {:?} {:?}\nA: {:?}\nB: {:?}",
                oa.np.phase(),
                oa.np.end(),
                ob.np.phase(),
                ob.np.end(),
                oa.watch.notes(),
                ob.watch.notes()
            );
            next += Duration::from_micros(FRAME_US);
            if let Some(d) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
        }
        assert!(
            replied,
            "観戦者が観戦用のコードを受けなかった: {:?}",
            shared_a.get().offer
        );
        assert_eq!(ga.recs.len(), FRAMES as usize);
        assert_eq!(ga.recs, gb.recs, "ルームで会った相手の試合の列が違う");
        let a_snap = oa.recv.spectate_snapshot().expect("ホストの観戦の集計");
        let c = a_snap.offer;
        assert!(c.armed && c.running, "{c:?}");
        assert!(c.entered >= 1 && c.offered >= 1, "{c:?}");
        assert_eq!((c.replies, c.punch_requested), (1, 1), "{c:?}");
        assert_eq!(a_snap.punch.started, 1, "{a_snap:?}");
        assert_eq!(a_snap.punch.sent, 5, "{a_snap:?}");
        assert_eq!(punches, 5, "観戦者の受信ソケットに届いた穴あけの印");
        assert!(oa.np.has_spectate(), "ホストがリングバッファを外した");
        let b_snap = ob.recv.spectate_snapshot().expect("ゲストの観戦の集計");
        assert_eq!(b_snap.offer.entered, 0, "{:?}", b_snap.offer);
        assert!(
            !b_snap.offer.running && b_snap.offer.armed,
            "{:?}",
            b_snap.offer
        );
        assert!(
            !ob.np.has_spectate(),
            "ゲストがリングバッファを外していない"
        );
        assert_eq!(b_snap.host.pushed, 0, "ゲストのリングバッファに積まれた");
        let (la, lb) = (oa.watch.notes().join("\n"), ob.watch.notes().join("\n"));
        assert!(la.contains("観戦の配る側を起こした"), "{la}");
        assert!(lb.contains("ゲストなので"), "{lb}");
        for l in [&la, &lb, &a_snap.text(), &b_snap.text()] {
            let low = l.to_lowercase();
            assert!(
                !low.contains("synth-watch-room")
                    && !low.contains(&spec_room.as_str().to_lowercase()),
                "{l}"
            );
        }
        let want = format!("/v0/rooms/{}", spec_room.as_str());
        let spec_paths = srv
            .seen
            .paths
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.starts_with("/v0/rooms/w."))
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            !spec_paths.is_empty() && spec_paths.iter().all(|p| p.starts_with(&want)),
            "{spec_paths:?}"
        );
        let mark = link_a.order_mark();
        let Opened {
            np,
            net,
            mut watch,
            recv,
            ..
        } = oa;
        let t = Instant::now();
        close_parts(np, net, &mut watch, recv, &link_a, mark);
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "片づけに {:?}",
            t.elapsed()
        );
        assert!(
            !shared_a.get().offer.running,
            "片づけの後も配る側のスレッドがある"
        );
        assert!(shared_a.offer_stopped());
        let _ = shared_b;
        drop(viewer);
    }

    // ---- 観戦者の組み上がり ----

    /// 観戦者を含む 3 台の組み上がりの共通の部分: 偽のルームサーバー ＋ 対戦者 2 台（A がホスト。ルームで会う。`[spectate] enable = 1`）
    /// ＋ 観戦者（偽のゲーム W ＋ 本物の `WatchPlay` ＋ 観戦者のルームのスレッド `spectate_join`）。`viewer_first` なら観戦者のルームの
    /// スレッドを対戦者より先に起こす（配る側がまだいない → 役 host → 閉じて 12 秒後に入り直し）。戻りは観戦者の状態の変わり目と、
    /// 各台の結果。
    struct Watched3 {
        ga: crate::netplay::fake::FakeGame,
        gb: crate::netplay::fake::FakeGame,
        gw: crate::netplay::fake::FakeGame,
        wp: crate::spectate_watch::WatchPlay,
        seen: Vec<(CodeState, Event)>,
        name_when_linking: String,
        counts: crate::spectate_join::JoinCounts,
        logs: String,
        offer: crate::spectate_offer::OfferCounts,
        /// 対戦のホストの名前（観戦者の左上の行の材料になる。ランダム対戦ではどちらがホストかは組ごとに決まる）。
        host_name: String,
        /// ランダム対戦の一覧から選ぶ組み上がりだけ: 観戦者が選んだ行・載った観戦者の数の最大・割り当てたルーム名。
        picked: Option<higanr_core::watch_list::WatchRow>,
        max_listed_watchers: u8,
        assigned: Vec<String>,
    }

    fn watch_through_rooms(viewer_first: bool, frames: u32, limit_secs: u64) -> Watched3 {
        watch_through(viewer_first, false, frames, limit_secs)
    }

    /// `random` なら、対戦者 2 台はランダム対戦で会い、観戦者は進行中のランダム対戦の一覧（偽サーバー）から行を選んで入る。そうでなければルームで会い、観戦者は
    /// ルーム名から観戦用のルームを導く。
    fn watch_through(viewer_first: bool, random: bool, frames: u32, limit_secs: u64) -> Watched3 {
        use crate::netplay::fake::{FRAME_US, FakeGame};
        assert!(
            !(random && viewer_first),
            "一覧から選ぶ観戦者は対戦が載ってから入る"
        );
        use crate::netplay::{self, CheckSource, Phase, WAIT_BUDGET_US};
        use crate::spectate_join::JoinKit;
        use crate::spectate_watch::{WATCH_MARGIN_FRAMES, WatchConfig, WatchPlay, WatchSlot};
        use higanr_core::wire::ByeReason;

        let _j = crate::spectate_join::tests::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::spectate_join::tests::reset_globals();
        let srv = FakeServer::start();
        let base = srv.base();
        let mk = |name: &str, extra: &str| {
            crate::net_config::from_text(&format!(
                "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\nname = {name}\ndelay = 3\nroom_server = {base}\n{extra}"
            ))
            .unwrap()
            .unwrap()
        };
        let (sa, sb) = (
            mk("alice", "[spectate]\nenable = 1\n"),
            mk("bob", "[spectate]\nenable = 1\n"),
        );
        let sv = mk("viewer", "");
        let cfg = netplay::Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            ..netplay::Config::default()
        };
        let (link_a, link_b, link_w) = (
            Arc::new(UiLink::new()),
            Arc::new(UiLink::new()),
            Arc::new(UiLink::new()),
        );
        let open = |s: &NetSettings, nonce: u64, link: &Arc<UiLink>| {
            open_set(
                s,
                cfg,
                nonce,
                T,
                quiet_notify,
                ModeCtx::none(),
                GameUi {
                    link: Arc::clone(link),
                    clip: Box::new(FakeClipboard::default()),
                    room: Some(room_opts(&base)),
                    room_last_dir: None,
                    spent: Vec::new(),
                },
            )
            .expect("ネットワークの組を開けない")
        };
        let mut oa = open(&sa, 0xF000, &link_a);
        let mut ob = open(&sb, 0x0F00, &link_b);
        let shared_a = oa.recv.spectate_shared().expect("ホストの観戦の共有");
        let shared_b = ob.recv.spectate_shared().expect("相手の観戦の共有");
        let base_view = base.clone();
        let (mut wnet, wrecv) = net_udp::open_for_code(sv.bind.fixed().unwrap()).unwrap();
        let viewer_addr = wnet.local_addr().unwrap();
        let slot = Arc::new(WatchSlot::new());
        let mut wp = WatchPlay::new(
            Arc::clone(&slot),
            WatchConfig {
                wait_budget_us: WAIT_BUDGET_US,
                margin_frames: WATCH_MARGIN_FRAMES,
                require_net_selection: false,
            },
        );
        let typed = "Synth-Watch-ROOM";
        let glyphs = ui_glyphs();
        let mut publisher = crate::spectate_join::Publisher::new(
            Arc::clone(&link_w),
            sanitize_room(typed, |c| glyphs.binary_search(&c).is_ok()),
            net_udp::now_unix() as u32,
        );
        publisher.publish(CodeState::SpectateJoining, Event::RoomJoining);
        // 最初の状態（開くスレッドが書く `SpectateJoining`）。見張りを起こす前に控える。
        let first_note = link_w.note().map(|n| (n.state, n.event));
        let kit = move |watch_room: RoomName| JoinKit {
            room: room_opts(&base),
            watch_room,
            reader: crate::spectate_watch_code::Reader::new(&sv, 0x77),
            reply_name: higanr_core::handshake::Name::new("viewer"),
            own: Candidates::from_addrs(&[viewer_addr]).unwrap(),
            slot,
            publisher,
        };
        let mut kit = Some(kit);
        let mut join_shared = None;
        if viewer_first {
            join_shared = Some(
                crate::spectate_join::spawn((kit.take().unwrap())(
                    RoomName::parse(typed).unwrap().spectate_of(),
                ))
                .unwrap(),
            );
        }
        // 一覧を見る観戦者（ランダム対戦のときだけ。メニューの一覧の画面と同じ見るエンドポイント）。
        let mut view = random.then(|| {
            crate::net_room::ViewListConn::start_view(&room_opts(&base_view), BUILD_NUMBER)
                .expect("一覧の見るエンドポイント")
        });
        let mut list_rows: Vec<higanr_core::watch_list::WatchRow> = Vec::new();
        let mut max_listed_watchers = 0u8;
        let mut picked: Option<higanr_core::watch_list::WatchRow> = None;
        link_a.send(if random {
            Cmd::Random
        } else {
            Cmd::Room(RoomBuf::from_name("synth-watch-room").unwrap())
        });
        let t0 = Instant::now();
        while link_a.note().map(|n| n.state) != Some(CodeState::RoomWaiting) {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "A がルームで待たない"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        link_b.send(if random {
            Cmd::Random
        } else {
            Cmd::Room(RoomBuf::from_name("SYNTH-WATCH-ROOM").unwrap())
        });
        let mut ga = FakeGame::new(0xA, frames, 1, 1);
        let mut gb = FakeGame::new(0xB, frames, 1, 1);
        let mut gw = FakeGame::new(0xC, frames, 1, 1);
        for g in [&mut ga, &mut gb, &mut gw] {
            g.menu_before_decide = 20;
        }
        let (mut closed_a, mut closed_b) = (false, false);
        let mut seen: Vec<(CodeState, Event)> = first_note.into_iter().collect();
        let mut name_when_linking = String::new();
        let mut after_end = 0u32;
        let t0 = Instant::now();
        let mut next = t0;
        loop {
            ga.do_frame(&mut oa.np, &mut oa.net);
            if ga.done && !closed_a {
                oa.np.close(ByeReason::Finished);
                closed_a = true;
            }
            gb.do_frame(&mut ob.np, &mut ob.net);
            if gb.done && !closed_b {
                ob.np.close(ByeReason::Finished);
                closed_b = true;
            }
            if let Some(v) = view.as_mut() {
                for ev in v.poll() {
                    if let higanr_core::watch_list::ViewEvent::List { rows, .. } = ev {
                        max_listed_watchers = rows
                            .iter()
                            .map(|r| r.watchers)
                            .fold(max_listed_watchers, u8::max);
                        list_rows = rows;
                    }
                }
            }
            let offered = shared_a.get().offer.offered + shared_b.get().offer.offered;
            if join_shared.is_none() && offered >= 1 {
                if random {
                    // メニューの一覧で行を選んだことにする（`Cmd::SpectateListed` → 観戦者のネットワークを開く仕事が `spectate_pick` から観戦用のルーム名を戻す）。
                    if let Some(row) = list_rows.first().copied() {
                        link_w.send(Cmd::SpectateListed(row));
                        let pick = link_w.spectate_pick().expect("選んだ行が入れ物にない");
                        picked = Some(pick);
                        join_shared = Some(
                            crate::spectate_join::spawn((kit.take().unwrap())(
                                pick.room_name().expect("一覧の行の観戦用のルーム名"),
                            ))
                            .unwrap(),
                        );
                    }
                } else {
                    join_shared = Some(
                        crate::spectate_join::spawn((kit.take().unwrap())(
                            RoomName::parse(typed).unwrap().spectate_of(),
                        ))
                        .unwrap(),
                    );
                }
            }
            if join_shared.is_some() {
                gw.do_frame(&mut wp, &mut wnet);
            }
            if let Some(n) = link_w.note()
                && seen.last() != Some(&(n.state, n.event))
            {
                seen.push((n.state, n.event));
                if n.state == CodeState::SpectateLinking && name_when_linking.is_empty() {
                    name_when_linking = n
                        .name
                        .iter()
                        .take_while(|&&u| u != 0)
                        .map(|&u| char::from_u32(u32::from(u)).unwrap_or('?'))
                        .collect();
                }
            }
            if wp.phase() == Phase::Ended && crate::spectate_join::finished() {
                after_end += 1;
                if after_end > 30 {
                    break;
                }
            }
            assert!(
                t0.elapsed() < Duration::from_secs(limit_secs),
                "終わらない: A {:?} / 観戦者 {:?} {:?} {:?}\nseen {seen:?}\nA: {:?}\n{}",
                oa.np.phase(),
                wp.phase(),
                wp.end(),
                wp.stats(),
                oa.watch.notes(),
                crate::spectate_join::stats_text()
            );
            next += Duration::from_micros(FRAME_US);
            if let Some(d) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
        }
        let counts = join_shared.as_ref().unwrap().counts();
        // ホスト（配る側が観戦用のルームへ入った方）。ルームで会う組み上がりは A、ランダム対戦は組ごと。
        let b_is_host = shared_b.get().offer.entered > shared_a.get().offer.entered;
        let host_name = if b_is_host { "bob" } else { "alice" };
        let offer = if b_is_host { &ob.recv } else { &oa.recv }
            .spectate_snapshot()
            .expect("ホストの観戦の集計")
            .offer;
        let assigned = lock_assigned(&srv);
        let logs = format!(
            "{}\n{}\n{}\n{}",
            oa.watch.notes().join("\n"),
            ob.watch.notes().join("\n"),
            counts.text(),
            crate::spectate_join::stats_text()
        );
        crate::spectate_join::stop_and_join();
        drop(wnet);
        let _ = wrecv.stop();
        // 配る側のスレッドを両方止める（ランダム対戦はどちらがホストか分からない。止めないと後も一覧と観戦用のルームへ入り直し続ける）。
        shared_a.stop_offer();
        shared_b.stop_offer();
        drop(view);
        let mark = link_a.order_mark();
        let Opened {
            np,
            net,
            mut watch,
            recv,
            ..
        } = oa;
        close_parts(np, net, &mut watch, recv, &link_a, mark);
        crate::spectate_join::tests::reset_globals();
        let _ = ob;
        Watched3 {
            ga,
            gb,
            gw,
            wp,
            seen,
            name_when_linking,
            counts,
            logs,
            offer,
            host_name: host_name.to_string(),
            picked,
            max_listed_watchers,
            assigned,
        }
    }

    /// 3 台の組み上がりの共通の合格: 観戦者の列がホスト（と対戦者）の列と全行一致・状態の変わり目の順・対戦者の名前・本ログにルーム名が出ない。
    fn assert_watched3(w: &Watched3, frames: u32) {
        assert_eq!(
            w.ga.recs.len(),
            frames as usize,
            "ホストの試合が最後まで進まない"
        );
        assert_eq!(w.ga.recs, w.gb.recs, "対戦者の列が食い違う");
        assert_eq!(
            w.gw.recs,
            w.ga.recs,
            "観戦者の列がホストの列と一致しない（観戦者 {} 行・ホスト {} 行）",
            w.gw.recs.len(),
            w.ga.recs.len()
        );
        assert_eq!(
            w.wp.end(),
            Some(crate::spectate_watch::WatchEnd::HostEnded(
                higanr_core::wire::ByeReason::Finished
            )),
            "{:?}",
            w.wp.stats()
        );
        let idx = |want: (CodeState, Event)| w.seen.iter().position(|s| *s == want);
        let joining = idx((CodeState::SpectateJoining, Event::RoomJoining));
        let linking = idx((CodeState::SpectateLinking, Event::SpectatePeer));
        let watching = idx((CodeState::SpectateIdle, Event::None));
        let ended = idx((CodeState::Idle, Event::SpectateEnded));
        assert!(
            matches!((joining, linking, watching, ended), (Some(a), Some(b), Some(c), Some(d)) if a < b && b < c && c < d),
            "状態の変わり目の順が違う: {:?}",
            w.seen
        );
        assert_eq!(
            w.name_when_linking, w.host_name,
            "対戦者の名前が左上の行の材料に載っていない"
        );
        let c = w.counts;
        assert_eq!(
            (c.codes_received, c.replies_sent, c.linked, c.ended),
            (1, 1, 1, 1),
            "{c:?}"
        );
        assert_eq!(
            c.code_unreadable + c.reply_failed + c.link_timeout + c.cancelled,
            0,
            "{c:?}"
        );
        assert!(
            w.offer.replies >= 1 && w.offer.punch_requested >= 1,
            "{:?}",
            w.offer
        );
        let low = w.logs.to_lowercase();
        assert!(!low.contains("synth-watch-room"), "{}", w.logs);
        assert!(
            !low.contains("w."),
            "観戦用のルームの名らしい文字が本ログに出た"
        );
        assert!(w.logs.contains("観戦者のルーム"), "{}", w.logs);
    }

    #[test]
    #[ignore = "見張りのスレッド・偽サーバー・本物の UDP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn a_menu_spectator_follows_the_room_hosts_match_through_the_spectate_room() {
        let w = watch_through_rooms(false, 420, 40);
        assert_watched3(&w, 420);
        assert_eq!(w.counts.wrong_role, 0, "配る側が先にいたのに役が逆になった");
    }

    #[test]
    #[ignore = "見張りのスレッド・偽サーバー・本物の UDP（loopback）で実時間に回す（観戦者の入り直しの 12 秒を含む。40 秒前後）。全件の回（build.bat）で --ignored で走る"]
    fn a_menu_spectator_that_arrives_first_waits_twelve_seconds_and_joins_mid_match() {
        let w = watch_through_rooms(true, 1500, 70);
        assert_watched3(&w, 1500);
        assert_eq!(w.counts.wrong_role, 1, "{:?}", w.counts);
        assert!(
            w.seen
                .contains(&(CodeState::SpectateWaiting, Event::RoomWaiting)),
            "配る側を待つ状態が出ていない: {:?}",
            w.seen
        );
    }

    #[test]
    #[ignore = "見張りのスレッド・偽サーバー・本物の UDP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn a_spectator_picks_a_random_match_from_the_list_and_follows_the_hosts_match() {
        let w = watch_through(false, true, 420, 40);
        // 観戦者の列がホスト（と対戦者）の列と全行一致・状態の変わり目の順・ホストの名前が観戦者の左上の行の材料になる。
        assert_watched3(&w, 420);
        assert_eq!(w.counts.wrong_role, 0, "配る側が先にいたのに役が逆になった");
        let row = w.picked.expect("一覧から行を選んでいない");
        let mut names = [row.p1.as_str(), row.p2.as_str()];
        names.sort_unstable();
        assert_eq!(names, ["alice", "bob"], "一覧の行の名前: {row:?}");
        assert_eq!(w.assigned.len(), 1, "組は 1 つ: {:?}", w.assigned);
        let q = RoomName::parse_server_rules(&w.assigned[0]).unwrap();
        assert_eq!(
            row.room_name(),
            Some(q.spectate_of()),
            "一覧の行から戻した観戦用のルーム名が、ホストの導く名と違う"
        );
        assert!(!row.is_full());
        // 観戦者が入ったことが一覧に届いた。
        assert!(
            w.max_listed_watchers >= 1,
            "観戦者が入っても一覧の観戦者の数が増えない: {}",
            w.max_listed_watchers
        );
        // 割り当てのルーム名・観戦用のルーム名は本ログに出ない。
        let low = w.logs.to_lowercase();
        assert!(!low.contains(&w.assigned[0]), "{}", w.logs);
        assert!(!low.contains(&w.assigned[0][2..]), "{}", w.logs);
        assert!(!low.contains(q.spectate_of().as_str()), "{}", w.logs);
    }

    #[test]
    #[ignore = "見張りのスレッド・偽サーバー・本物の UDP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn two_random_match_players_with_spectate_disabled_never_touch_the_list() {
        let srv = FakeServer::start();
        let (logs, _, _, listed) = play_two_games(&srv, "", Cmd::Random, Cmd::Random);
        assert!(
            listed.is_empty(),
            "観戦を使わない設定のホストが載った: {listed:?}"
        );
        let paths = srv.seen.paths.lock().unwrap().clone();
        assert!(
            paths.iter().all(|p| !p.starts_with("/v0/watch/")),
            "一覧のエンドポイントへ触った: {paths:?}"
        );
        assert_eq!(
            srv.seen
                .watch_post_upgrades
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
        assert!(!logs.contains("一覧に載せる"), "{logs}");
    }

    #[test]
    #[ignore = "見張りのスレッド・偽サーバー・本物の UDP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
    fn a_random_match_host_does_not_list_when_only_the_guest_disables_spectate() {
        let srv = FakeServer::start();
        // A（nonce の大きい側 = ホスト）だけが `[spectate] enable = 1`。ゲストの HELLO の印で、ホストは載せず配る側も起こさない。
        let (logs, _, _, listed) = play_two_games_with(
            &srv,
            "[spectate]\nenable = 1\n",
            "[spectate]\nenable = 0\n",
            Cmd::Random,
            Cmd::Random,
        );
        // ホストが決める所を通った（通らずに載らなかったのではない）。
        assert!(
            logs.contains("ランダムマッチの相手が一覧に載せない設定"),
            "{logs}"
        );
        assert!(listed.is_empty(), "ゲストが断ったのに載った: {listed:?}");
        let paths = srv.seen.paths.lock().unwrap().clone();
        assert!(
            paths.iter().all(|p| !p.starts_with("/v0/watch/")),
            "一覧のエンドポイントへ触った: {paths:?}"
        );
        assert_eq!(
            srv.seen
                .watch_post_upgrades
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
        assert!(!logs.contains("観戦の配る側を起こした"), "{logs}");
    }
}
