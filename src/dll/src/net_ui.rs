//! ゲーム内メニュー（`code_side = game`）のゲームのスレッドの側（pure。unsafe無し）。
//!
//! 判断はcoreが行う（`higanr_core::ui_entry`が入口の判定、`net_menu`がステートマシンと固定の文、`ui_layout`が頂点の配置）。
//! ここはその判断をゲームのスレッドで1 tickに1回ずつ進める接続役。
//! 監視スレッドとはアトミック変数だけで通信し、描く行を「盤」に書き込む。
//! - **受け渡し（確保・ロック待ち無し）**: ゲームのスレッドから監視スレッドへは、指示のリングバッファ`UiLink::send`と対戦の形の値で渡す。
//!   監視スレッドからゲームのスレッドへは、状態`Note`（種類と数。文字列を持たない）をタグつきのバッファ`NoteBox`で渡す。
//!   ゲームのスレッドから描画の入口へは、描く行の盤`Board`（`draw_board`が描く）で渡す。
//! - **ゲームの入力**: Matchの画面（6）の間は入力の地点（`net_input`）でこのUIが入力を占有する。
//!   メニューが閉じていれば素のpollの3本をそのままfeedする（`ui_entry::leaks`が漏れると判定したtickだけ0を3本feedする）。
//!   開いていれば0を3本feedする。画面6以外ではゲームの入力に触らない。
//! - **遅延の増分**: 画面6のtickだけ（素のpollを3回 ＝ `poll_all`が元から呼ぶ回数と同じ）。
//!   入力 → Presentの間に足すのは、描画の入口の盤の読みだけ（アトミック変数の読みと、メニューが開いている間の頂点100行ほど）。
//! - ルーム名・プレイヤー名の入力、観戦者のセッション、観戦コードのコピー（Ctrl+C）、接続一式の作り直しはこの下の各関数にある。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};
use std::sync::Arc;

use higanr_core::delay_pick::DelayChoice;
use higanr_core::handshake::Name;
use higanr_core::input_model::{InputState, keys};
use higanr_core::lan_addr::{AddrBuf, LanAddr};
use higanr_core::lang::{Lang, Tx};
use higanr_core::menu_model::{CANCEL_BITS, SCREEN_DIFFICULTY};
use higanr_core::mode_pick::{MAX_PICK_DELAY, Pick, Shape};
use higanr_core::net_menu::return_lines;
use higanr_core::net_menu::{
    Answer, Cmd, Conn, Event, HudView, LineBuf, MENU_ROWS, Menu, MenuRows, NOTE_WORDS, Note,
    PeerStats, RewindWindow, RoomItem, STALL_ROWS, Stall, T_PEER_WAIT, T_SPECTATE_ESC_HOLD,
    T_SPECTATE_NEXT_SELECT, T_SPECTATE_PAUSED, TextIn, TickIn, UPDATE_NOTICE_LINES, WatchHudView,
    copy_hint, copy_notice, corner_is_band, corner_lines, hud_line, name_band_label_range,
    name_band_line, stall_block, update_notice_lines, watch_hud_line, watch_off_hint,
};
use higanr_core::overlay::{AtlasSpec, MAX_TEXT_UTF8, Vertex, utf16_to_utf8};
use higanr_core::player_name::{NAME_BUF_WORDS, NameBuf};
use higanr_core::return_flow::{self, Facts, Finish, Flow, Reads, Screen, Why};
use higanr_core::room_input::{ROOM_BUF_WORDS, RoomBuf};
use higanr_core::split_kb::{self, DeviceTable};
use higanr_core::status_side::StatusSide;
use higanr_core::ui_entry::{self, Advance, EntryView, SCREEN_MATCH_MODE};
use higanr_core::ui_layout::{self as lay, HUD_BAND_H, HUD_TEXT_MAX_W, HUD_X, HUD_Y, RowStyle};
use higanr_core::update_notice::{Found, Poll, Trigger};
use higanr_core::vpn_net::VpnSet;
use higanr_core::watch_list::{
    ListStatus, WATCH_ROW_WORDS, WATCH_ROWS_MAX, WatchRow, WatchSnapshot,
};
use higanr_core::wire::{ByeReason, Mode, Seat, SeatWish};

use crate::digit_keys::{self, DigitEdge};
use crate::netplay::REQ_SAVE_AND_QUIT;
use crate::netplay::{Action, Game, Net, NetPlay, Phase, SCENE_MATCH};
use crate::overlay::{TEXT_UNITS, TextBox};
use crate::scripted::SCENE_MENU;
use crate::spectate_watch::{WatchNotice, WatchPlay};

// ---- 監視スレッドとのインターフェース ----

/// 監視スレッドへの指示の種類（`UiLink::send`）。`Cmd::Shape`は別のフィールド（`shape`）で渡す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Create,
    Paste,
    Cancel,
    /// ルームサーバーのルームで待つ。
    Room,
    /// 観戦者として観戦用のルームで待つ（ルーム名は`UiLink::spectate_name`。`Room`の名のバッファとは別）。
    Spectate,
    /// 観戦の待ちを取りやめる。
    /// `Cancel`は対戦する側の待ちの取りやめなので受け手が取り違えないよう別の指示にしている。
    SpectateCancel,
    /// ランダムマッチの待ち列に入る（名のバッファは使わない: ルーム名はサーバーが割り当てる）。
    Random,
    /// 観戦コードをコピー（対戦の最中。ホストの監視スレッドがクリップボードへ書く）。
    SpectateCopy,
}

impl Order {
    fn code(self) -> u32 {
        match self {
            Order::Create => 1,
            Order::Paste => 2,
            Order::Cancel => 3,
            Order::Room => 4,
            Order::Spectate => 5,
            Order::SpectateCancel => 6,
            Order::Random => 7,
            Order::SpectateCopy => 8,
        }
    }

    fn from_code(c: u32) -> Option<Order> {
        match c {
            1 => Some(Order::Create),
            2 => Some(Order::Paste),
            3 => Some(Order::Cancel),
            4 => Some(Order::Room),
            5 => Some(Order::Spectate),
            6 => Some(Order::SpectateCancel),
            7 => Some(Order::Random),
            8 => Some(Order::SpectateCopy),
            _ => None,
        }
    }
}

/// 指示のリングバッファの長さ（2の冪）。人の操作は1 tickに1つ以下。監視スレッドは数十msごとに取るので溢れない（溢れたら新しい指示を捨てる）。
const ORDER_RING: usize = 8;

/// ゲームのスレッドと監視スレッドの間のインターフェース。書き手と読み手は各1人（指示はゲームのスレッドが書き監視スレッドが読む。状態は逆）。
pub struct UiLink {
    /// 指示のリングバッファ。書き手（ゲームのスレッド）が`head`をReleaseで進める。読み手（監視スレッド）はAcquireで読んでから中身を読む。
    orders: [AtomicU32; ORDER_RING],
    head: AtomicU32,
    tail: AtomicU32,
    /// 対戦の形（メニューで変えた値）。0 = 変えていない。1 + `Pick` の番号。
    shape: AtomicU32,
    /// 対戦の遅延（`Cmd::Shape`の後半）。0 = 変えていない。1 + `DelayChoice::raw`（autoは `AUTO` = 9）。
    delay: AtomicU32,
    /// サイド（メニューで変えたサイドの希望）。0 = 変えていない。1 + `seat_code` の番号。
    seat: AtomicU32,
    note: NoteBox,
    /// ルーム名の入力で決定した名（`Order::Room`を積む前に書く。監視スレッドがその指示を取るときに読む）。
    room_name: RoomNameBox,
    /// 観戦の「ルームマッチ」の入力で決定した名（`Order::Spectate`を積む前に書く）。対戦の「ルームマッチ」の名と入れ違わないよう別のバッファにする。
    spectate_name: RoomNameBox,
    /// メニューで決定したプレイヤー名（`Cmd::Name`）。決定していなければ空になり、ネットワークを開くスレッドはiniの名前を使う。
    /// 書き手はゲームのスレッド。読み手はネットワークを開くスレッドと監視スレッド。
    player_name: PlayerNameBox,
    /// 観戦の「ランダムマッチ」の一覧（書き手は一覧の接続のスレッド`watch_feed`。読み手はゲームのスレッド）。
    watch_list: WatchListBox,
    /// 一覧の接続の世代（`watch_feed`。止めた古いスレッドが次に開いた一覧のバッファへ書かないための印）。
    watch_gen: AtomicU32,
    /// 一覧から選んだ行（`Cmd::SpectateListed`。`Order::Spectate`を積む前に書く）。読み手は観戦者のネットワークを開く仕事。
    /// 観戦の「ルームマッチ」を決定すると空にする（古い選択が入力した名を奪わないように）。
    spectate_pick: WatchRowBox,
    /// 観戦の決定が「観戦コード入力」かどうか（1 = 観戦コード。0 = ルーム名か一覧の行）。`Order::Spectate`を積む前に書く。
    spectate_by_code: AtomicU32,
}

impl Default for UiLink {
    fn default() -> Self {
        UiLink::new()
    }
}

/// `Pick`の番号（rollbackのDは0〜`MAX_PICK_DELAY` = 8 がその数。delay方式は9）。
fn pick_code(p: Pick) -> u32 {
    match p {
        Pick::Rollback(d) => u32::from(d.min(MAX_PICK_DELAY)),
        Pick::Delay => u32::from(MAX_PICK_DELAY) + 1,
    }
}

/// `SeatWish`の番号（0〜2。`net_menu`の並びと同じ: 1P・2P・おまかせ）。
fn seat_code(w: SeatWish) -> u32 {
    match w {
        SeatWish::P1 => 0,
        SeatWish::P2 => 1,
        SeatWish::Any => 2,
    }
}

fn seat_of(c: u32) -> Option<SeatWish> {
    match c {
        0 => Some(SeatWish::P1),
        1 => Some(SeatWish::P2),
        2 => Some(SeatWish::Any),
        _ => None,
    }
}

fn pick_of(c: u32) -> Option<Pick> {
    let max = u32::from(MAX_PICK_DELAY);
    if c <= max {
        Some(Pick::Rollback(c as u8))
    } else if c == max + 1 {
        Some(Pick::Delay)
    } else {
        None
    }
}

impl UiLink {
    pub fn new() -> UiLink {
        UiLink {
            orders: [const { AtomicU32::new(0) }; ORDER_RING],
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
            shape: AtomicU32::new(0),
            delay: AtomicU32::new(0),
            seat: AtomicU32::new(0),
            note: NoteBox::new(),
            room_name: RoomNameBox::new(),
            spectate_name: RoomNameBox::new(),
            player_name: PlayerNameBox::new(),
            watch_list: WatchListBox::new(),
            watch_gen: AtomicU32::new(0),
            spectate_pick: WatchRowBox::new(),
            spectate_by_code: AtomicU32::new(0),
        }
    }

    pub fn send(&self, c: Cmd) {
        match c {
            // 名前は指示のリングバッファを通さない（取りやめる・作るなどの順序に関わらない値。次にネットワーク・コードを作る側がバッファから読む）。
            Cmd::Name(n) => self.player_name.publish(&n),
            // 設定のルーム名は監視スレッドへの指示ではない（監視スレッドは最初の「ルームマッチ」まで存在しない）。保存するのは`NetUi::room_saver`が作る別のスレッド。
            Cmd::RoomSet(_) => {}
            Cmd::Shape(p, d) => {
                // 遅延を先に設定する。監視スレッドは形を読んだ後に遅延を読む。形だけ新しくて遅延が古い組み合わせを見せない順序にしている。
                self.delay.store(1 + u32::from(d.raw()), Ordering::Release);
                self.shape.store(1 + pick_code(p), Ordering::Release);
            }
            // サイドも形と同じく指示のリングバッファを通さない（監視スレッドがセッションの設定を登録するときに読む値）。
            Cmd::Seat(w) => self.seat.store(1 + seat_code(w), Ordering::Release),
            // 言語は監視スレッドに関係しない（iniへ書くのは`NetUi::lang_saver`が作る別のスレッド）。
            Cmd::Lang(_) => {}
            // LANの申し込みは監視スレッドへの指示ではない（監視スレッドはまだ存在しない）。
            // ゲームのスレッドが`UiOut::lan`で呼び手へ頼み、LANの運び手のスレッドが自分で指示のリングバッファを読む。
            Cmd::LanHost | Cmd::LanClient(_) | Cmd::LanWatch(_) => {}
            Cmd::Create => self.push(Order::Create),
            Cmd::Paste => self.push(Order::Paste),
            Cmd::Cancel => self.push(Order::Cancel),
            Cmd::Random => self.push(Order::Random),
            Cmd::Room(name) => {
                self.room_name.publish(&name);
                self.push(Order::Room);
            }
            Cmd::Spectate(name) => {
                self.spectate_name.publish(&name);
                self.spectate_pick.publish(None);
                self.spectate_by_code.store(0, Ordering::Release);
                self.push(Order::Spectate);
            }
            Cmd::SpectateListed(row) => {
                self.spectate_pick.publish(Some(&row));
                self.spectate_by_code.store(0, Ordering::Release);
                self.push(Order::Spectate);
            }
            Cmd::SpectateCode => {
                self.spectate_pick.publish(None);
                self.spectate_by_code.store(1, Ordering::Release);
                self.push(Order::Spectate);
            }
            Cmd::SpectateCancel => self.push(Order::SpectateCancel),
            Cmd::CopySpectateCode => self.push(Order::SpectateCopy),
        }
    }

    /// 監視スレッド: `Order::Room`に添えられたルーム名（最後に決定した名。書きかけなら読み直す。まだ決定していなければ`None`）。
    pub fn room_name(&self) -> Option<RoomBuf> {
        (0..4).find_map(|_| self.room_name.read())
    }

    /// メニューで決定したプレイヤー名（最後に決定した名。まだ決定していなければ`None`）。
    /// ネットワークを開くスレッドと監視スレッドが次に名乗る名前を決めるときに読む。
    pub fn player_name(&self) -> Option<NameBuf> {
        (0..4).find_map(|_| self.player_name.read())
    }

    /// `Cmd::Spectate`に添えられたルーム名（`room_name`と同じ読み方。別のバッファ）。観戦者のネットワークを開く仕事（`script_win::open_watch_net`）が読む。
    pub fn spectate_name(&self) -> Option<RoomBuf> {
        (0..4).find_map(|_| self.spectate_name.read())
    }

    /// 観戦者のネットワークを開く仕事が読む: 最後の観戦の決定が「観戦コード入力」かどうか（真ならクリップボードの観戦コードでホストへ直接接続する）。
    pub fn spectate_by_code(&self) -> bool {
        self.spectate_by_code.load(Ordering::Acquire) != 0
    }

    /// 一覧から選んだ行（選んでいなければ`None`。その場合は`spectate_name`を使う）。観戦者のネットワークを開く仕事が読む。
    pub fn spectate_pick(&self) -> Option<WatchRow> {
        (0..4).find_map(|_| self.spectate_pick.read()).flatten()
    }

    /// 一覧の接続のスレッド: 観戦の「ランダムマッチ」の一覧を書く（書き手は1つのスレッド）。
    pub fn publish_watch_list(&self, status: ListStatus, rows: &[WatchRow]) {
        self.watch_list.publish(status, rows);
    }

    /// 一覧の接続の世代を1つ進めて、進めた後の値を返す（開く・止めるたびに）。
    pub fn next_watch_generation(&self) -> u32 {
        self.watch_gen
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    }

    /// いまの一覧の接続の世代。
    pub fn watch_generation(&self) -> u32 {
        self.watch_gen.load(Ordering::Acquire)
    }

    /// 一覧のバージョン（書くたびに進む）。ゲームのスレッドが一覧の画面に入った時点の値を覚えて「それより新しいものだけ」を読むために使う。
    pub fn watch_list_version(&self) -> u32 {
        self.watch_list.version()
    }

    /// ゲームのスレッド: `seen`より新しい一覧があれば`scratch`へ読み、`seen`を進めてtrueを返す。書きかけか変わっていなければfalseを返す（`scratch`は使わない）。
    pub fn read_watch_list(&self, scratch: &mut WatchSnapshot, seen: &mut u32) -> bool {
        self.watch_list.read_newer(scratch, seen)
    }

    fn push(&self, o: Order) {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) as usize >= ORDER_RING {
            return;
        }
        if let Some(slot) = self.orders.get(head as usize % ORDER_RING) {
            slot.store(o.code(), Ordering::Relaxed);
        }
        self.head.store(head.wrapping_add(1), Ordering::Release);
    }

    pub fn take_order(&self) -> Option<Order> {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail == self.head.load(Ordering::Acquire) {
            return None;
        }
        let code = self
            .orders
            .get(tail as usize % ORDER_RING)
            .map_or(0, |s| s.load(Ordering::Relaxed));
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Order::from_code(code)
    }

    pub fn shape(&self) -> Option<Pick> {
        match self.shape.load(Ordering::Acquire) {
            0 => None,
            c => pick_of(c - 1),
        }
    }

    /// 監視スレッド: メニューで選んだ遅延（変えていなければ`None`。接続の始まりにホストが押下にする）。
    pub fn start_delay(&self) -> Option<DelayChoice> {
        match self.delay.load(Ordering::Acquire) {
            0 => None,
            c => u8::try_from(c - 1).ok().and_then(DelayChoice::from_raw),
        }
    }

    pub fn seat(&self) -> Option<SeatWish> {
        match self.seat.load(Ordering::Acquire) {
            0 => None,
            c => seat_of(c - 1),
        }
    }

    pub fn publish(&self, n: &Note) {
        self.note.publish(n);
    }

    /// ゲームのスレッド: ここまでに送った指示の数（後始末のときの目印。`drop_orders_before`へ渡す）。アトミック変数を1つ読むだけ。
    pub fn order_mark(&self) -> u32 {
        self.head.load(Ordering::Acquire)
    }

    /// 再接続の後始末（ワーカースレッド。**前の監視スレッドを止めて待った後、次の監視スレッドを開始する前だけ**呼ぶ）。
    /// 前の監視スレッドが取り残した指示（`mark`より前に送られたもの）を捨てて、状態を「何も無い」にする。
    /// `mark`より後の指示（後始末している間に人が選んだ「作る」など）は残し、次の監視スレッドが取る。
    pub fn reset_for_next_watcher(&self, mark: u32) {
        let tail = self.tail.load(Ordering::Relaxed);
        // `mark`が`tail`より前（もう取った）なら動かさない。リングバッファの長さを越える差は来ない（指示は1 tickに1つ以下）。
        if mark.wrapping_sub(tail) as usize <= ORDER_RING {
            self.tail.store(mark, Ordering::Release);
        }
        let seq = self.note().map_or(0, |n| n.seq);
        self.note.publish(&Note {
            seq: seq.wrapping_add(1),
            ..Note::default()
        });
    }

    /// 観戦者のセッションの後始末（ワーカースレッド。監視スレッドが存在しない間だけ呼ぶ）。
    /// `mark`より前に送られた指示を捨てる。状態は消さない（観戦の終わりの理由を左上に残す）。
    /// 観戦の「ルームマッチ」の指示はネットワークを開く仕事（`UiOut::spectate_open`）が受けるので、リングバッファからは誰も取らない。
    /// 溜まったままにすると新しい指示（対戦する側の「作る」）を捨ててしまう。
    pub fn drop_orders_before(&self, mark: u32) {
        let tail = self.tail.load(Ordering::Relaxed);
        if mark.wrapping_sub(tail) as usize <= ORDER_RING {
            self.tail.store(mark, Ordering::Release);
        }
    }

    /// ゲームのスレッド: 最新の状態（書きかけなら`None`。呼び手は前の値を使い続ける）。
    pub fn note(&self) -> Option<Note> {
        self.note.read()
    }
}

/// 状態のタグつきのバッファ（seqlock。書き手は1人。読み手は待たない: 書きかけなら`None`）。
pub struct NoteBox {
    seq: AtomicU32,
    words: [AtomicU32; NOTE_WORDS],
}

impl NoteBox {
    pub const fn new() -> NoteBox {
        NoteBox {
            seq: AtomicU32::new(0),
            words: [const { AtomicU32::new(0) }; NOTE_WORDS],
        }
    }

    pub fn publish(&self, n: &Note) {
        let w = n.to_words();
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (d, v) in self.words.iter().zip(w) {
            d.store(v, Ordering::Relaxed);
        }
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    pub fn read(&self) -> Option<Note> {
        let s1 = self.seq.load(Ordering::Acquire);
        if s1 & 1 != 0 || s1 == 0 {
            return None;
        }
        let mut w = [0u32; NOTE_WORDS];
        for (d, v) in w.iter_mut().zip(&self.words) {
            *d = v.load(Ordering::Relaxed);
        }
        fence(Ordering::Acquire);
        (self.seq.load(Ordering::Relaxed) == s1).then(|| Note::from_words(&w))
    }
}

impl Default for NoteBox {
    fn default() -> Self {
        NoteBox::new()
    }
}

/// ルーム名のタグつきのバッファ（`NoteBox`と同じseqlock）。ルーム名は本ログに書かない（`RoomBuf`の`Debug`も名を出さない）。
pub struct RoomNameBox {
    seq: AtomicU32,
    words: [AtomicU32; ROOM_BUF_WORDS],
}

impl RoomNameBox {
    pub const fn new() -> RoomNameBox {
        RoomNameBox {
            seq: AtomicU32::new(0),
            words: [const { AtomicU32::new(0) }; ROOM_BUF_WORDS],
        }
    }

    pub fn publish(&self, name: &RoomBuf) {
        let w = name.to_words();
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (d, v) in self.words.iter().zip(w) {
            d.store(v, Ordering::Relaxed);
        }
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    pub fn read(&self) -> Option<RoomBuf> {
        let s1 = self.seq.load(Ordering::Acquire);
        if s1 & 1 != 0 || s1 == 0 {
            return None;
        }
        let mut w = [0u32; ROOM_BUF_WORDS];
        for (d, v) in w.iter_mut().zip(&self.words) {
            *d = v.load(Ordering::Relaxed);
        }
        fence(Ordering::Acquire);
        (self.seq.load(Ordering::Relaxed) == s1).then(|| RoomBuf::from_words(&w))
    }
}

impl Default for RoomNameBox {
    fn default() -> Self {
        RoomNameBox::new()
    }
}

/// プレイヤー名のタグつきのバッファ（`RoomNameBox`と同じseqlock）。
pub struct PlayerNameBox {
    seq: AtomicU32,
    words: [AtomicU32; NAME_BUF_WORDS],
}

impl PlayerNameBox {
    pub const fn new() -> PlayerNameBox {
        PlayerNameBox {
            seq: AtomicU32::new(0),
            words: [const { AtomicU32::new(0) }; NAME_BUF_WORDS],
        }
    }

    pub fn publish(&self, name: &NameBuf) {
        let w = name.to_words();
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (d, v) in self.words.iter().zip(w) {
            d.store(v, Ordering::Relaxed);
        }
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    pub fn read(&self) -> Option<NameBuf> {
        let s1 = self.seq.load(Ordering::Acquire);
        if s1 & 1 != 0 || s1 == 0 {
            return None;
        }
        let mut w = [0u32; NAME_BUF_WORDS];
        for (d, v) in w.iter_mut().zip(&self.words) {
            *d = v.load(Ordering::Relaxed);
        }
        fence(Ordering::Acquire);
        (self.seq.load(Ordering::Relaxed) == s1).then(|| NameBuf::from_words(&w))
    }
}

impl Default for PlayerNameBox {
    fn default() -> Self {
        PlayerNameBox::new()
    }
}

/// 一覧のバッファのバージョンの下の桁に使うワードの数: 状態と行数の1ワード ＋ 行のワード。
const WATCH_BOX_WORDS: usize = 1 + WATCH_ROWS_MAX * WATCH_ROW_WORDS;

/// 観戦の「ランダムマッチ」の一覧のタグつきのバッファ（`NoteBox`と同じseqlock。書き手は1人）。読み手は待たない: 書きかけなら読まない。
pub struct WatchListBox {
    seq: AtomicU32,
    words: [AtomicU32; WATCH_BOX_WORDS],
}

impl WatchListBox {
    pub fn new() -> WatchListBox {
        WatchListBox {
            seq: AtomicU32::new(0),
            words: [const { AtomicU32::new(0) }; WATCH_BOX_WORDS],
        }
    }

    /// 偶数のバージョンは書き終えた状態。書くたびに2進む。
    pub fn version(&self) -> u32 {
        self.seq.load(Ordering::Acquire) & !1
    }

    pub fn publish(&self, status: ListStatus, rows: &[WatchRow]) {
        let n = rows.len().min(WATCH_ROWS_MAX);
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        if let Some(head) = self.words.first() {
            head.store(Self::meta(status, n), Ordering::Relaxed);
        }
        for (i, row) in rows.iter().take(n).enumerate() {
            for (d, v) in self
                .words
                .iter()
                .skip(1 + i * WATCH_ROW_WORDS)
                .zip(row.to_words())
            {
                d.store(v, Ordering::Relaxed);
            }
        }
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    fn meta(status: ListStatus, n: usize) -> u32 {
        let st = match status {
            ListStatus::Loading => 0,
            ListStatus::Ready => 1,
            ListStatus::Failed => 2,
        };
        st | ((n as u32) << 8)
    }

    /// `seen`より新しい（偶数の）バージョンが書き終わっていれば`out`へ読んでtrueを返す。読んでいる最中に書き手が動いたらfalseを返す（`out`は呼び手が捨てる）。
    pub fn read_newer(&self, out: &mut WatchSnapshot, seen: &mut u32) -> bool {
        let s1 = self.seq.load(Ordering::Acquire);
        if s1 & 1 != 0 || s1 == *seen || s1 == 0 {
            return false;
        }
        let meta = self.words.first().map_or(0, |w| w.load(Ordering::Relaxed));
        let n = ((meta >> 8) as usize).min(WATCH_ROWS_MAX);
        let status = match meta & 0xFF {
            1 => ListStatus::Ready,
            2 => ListStatus::Failed,
            _ => ListStatus::Loading,
        };
        out.clear();
        let mut ok = true;
        for i in 0..n {
            let mut w = [0u32; WATCH_ROW_WORDS];
            for (d, v) in w
                .iter_mut()
                .zip(self.words.iter().skip(1 + i * WATCH_ROW_WORDS))
            {
                *d = v.load(Ordering::Relaxed);
            }
            match WatchRow::from_words(&w) {
                Some(r) => {
                    ok &= out.push(r);
                }
                // 書き手が書いている途中のワード（バージョンが変わる前の読み）。バージョンのチェックで捨てる。
                None => ok = false,
            }
        }
        fence(Ordering::Acquire);
        if !ok || self.seq.load(Ordering::Relaxed) != s1 {
            return false;
        }
        out.status = status;
        *seen = s1;
        true
    }
}

impl Default for WatchListBox {
    fn default() -> Self {
        WatchListBox::new()
    }
}

/// 一覧から選んだ行1つのタグつきのバッファ（`NoteBox`と同じseqlock）。`None`を書くと空になる（行のワードが全部 0 = 上限0 は行として読めない）。
pub struct WatchRowBox {
    seq: AtomicU32,
    words: [AtomicU32; WATCH_ROW_WORDS],
}

impl WatchRowBox {
    pub const fn new() -> WatchRowBox {
        WatchRowBox {
            seq: AtomicU32::new(0),
            words: [const { AtomicU32::new(0) }; WATCH_ROW_WORDS],
        }
    }

    pub fn publish(&self, row: Option<&WatchRow>) {
        let w = row.map_or([0u32; WATCH_ROW_WORDS], WatchRow::to_words);
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (d, v) in self.words.iter().zip(w) {
            d.store(v, Ordering::Relaxed);
        }
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// 読み取れたら`Some(設定された行 or 空)`。書きかけなら`None`。
    pub fn read(&self) -> Option<Option<WatchRow>> {
        let s1 = self.seq.load(Ordering::Acquire);
        if s1 & 1 != 0 {
            return None;
        }
        if s1 == 0 {
            return Some(None);
        }
        let mut w = [0u32; WATCH_ROW_WORDS];
        for (d, v) in w.iter_mut().zip(&self.words) {
            *d = v.load(Ordering::Relaxed);
        }
        fence(Ordering::Acquire);
        (self.seq.load(Ordering::Relaxed) == s1).then(|| WatchRow::from_words(&w))
    }
}

impl Default for WatchRowBox {
    fn default() -> Self {
        WatchRowBox::new()
    }
}

// ---- 描く行の盤 ----

/// 盤の行の割り当て: メニューの行（`MENU_ROWS`）・左上の2行（警告のときは続きの2行も）・マッチ中の行。
pub const ROW_TL0: usize = MENU_ROWS;
pub const ROW_TL1: usize = MENU_ROWS + 1;
pub const ROW_HUD: usize = MENU_ROWS + 2;
/// 待っている間の1行（ルームで待つ・接続コードで待つ。`net_menu::corner_is_band`）。マッチ中の行と同じ置き場・文字で描く。
pub const ROW_WAIT: usize = MENU_ROWS + 3;
/// 試合中の名前の帯（上の枠の左に1P、右に2P）。
pub const ROW_NAME_L: usize = MENU_ROWS + 4;
pub const ROW_NAME_R: usize = MENU_ROWS + 5;
/// 左上の警告の3〜5行目（`net_menu::STALL_ROWS` = 5行）。
pub const ROW_TL2: usize = MENU_ROWS + 6;
pub const ROW_TL3: usize = MENU_ROWS + 7;
pub const ROW_TL4: usize = MENU_ROWS + 8;
/// 相手を待つ知らせの1行（`NetPlay::peer_wait_shown`）。左上の行の続きに描く。
pub const ROW_PEER_WAIT: usize = MENU_ROWS + 9;
/// 接続している間の観戦コードの知らせ（対戦のホストへの案内とCtrl+Cの結果）の1行。左上の行の続きに描く。
pub const ROW_COPY: usize = MENU_ROWS + 10;
/// 新しいバージョンの知らせの2行（`net_menu::update_notice_lines`）。マッチ終了時のメニューが出ている間は左上の行の続きに描く。
pub const ROW_UPD0: usize = MENU_ROWS + 11;
pub const ROW_UPD1: usize = MENU_ROWS + 12;
pub const BOARD_ROWS: usize = MENU_ROWS + 13;
pub const SHOW_MENU: u32 = 1;
pub const SHOW_TL0: u32 = 2;
pub const SHOW_TL1: u32 = 4;
/// マッチ中の行（通信の状態。下の枠の左）。
pub const SHOW_HUD: u32 = 8;
pub const SHOW_WAIT: u32 = 16;
/// 試合中の名前の帯（`ROW_NAME_L` / `ROW_NAME_R`）。
pub const SHOW_NAMES: u32 = 32;
/// 左上の警告の3〜5行目（`ROW_TL2` / `ROW_TL3` / `ROW_TL4`）。
pub const SHOW_TL2: u32 = 64;
pub const SHOW_TL3: u32 = 128;
pub const SHOW_TL4: u32 = 256;
pub const SHOW_PEER_WAIT: u32 = 512;
/// 観戦コードの知らせ（`ROW_COPY`）。
pub const SHOW_COPY: u32 = 1024;
/// 新しいバージョンの知らせ（`ROW_UPD0` / `ROW_UPD1`）。
pub const SHOW_UPD0: u32 = 2048;
pub const SHOW_UPD1: u32 = 4096;
const SHOW_UPDATE: u32 = SHOW_UPD0 | SHOW_UPD1;
/// 左上の行のフラグと行のペア（上から順）。
const TL_ROWS: [(u32, usize); STALL_ROWS] = [
    (SHOW_TL0, ROW_TL0),
    (SHOW_TL1, ROW_TL1),
    (SHOW_TL2, ROW_TL2),
    (SHOW_TL3, ROW_TL3),
    (SHOW_TL4, ROW_TL4),
];
/// マッチ中だけ出るフラグ（`hud`が設定する。メニュー・左上の行の更新が残す側）。
const SHOW_MATCH: u32 = SHOW_HUD | SHOW_NAMES;
/// メニュー・左上の行の更新が消さずに残すフラグ。
/// 相手を待つ知らせは`peer_wait`だけが、観戦コードの知らせは`copy_tick`だけが、新しいバージョンの知らせは`update_tick`だけが決める。
const SHOW_KEPT: u32 = SHOW_MATCH | SHOW_PEER_WAIT | SHOW_COPY | SHOW_UPDATE;

/// 左上の行の置き場（画素。ゲームの上の枠の下から）。マッチ中の行の置き場と幅の上限は`ui_layout`の`HUD_*`。
const TL_X: u32 = 6;
const TL_Y: u32 = 20;

/// 描く行の盤。書き手は1つのスレッド（ゲームのスレッド）。読み手は描画の入口（同じスレッド）と検査。
pub struct Board {
    rows: [TextBox; BOARD_ROWS],
    styles: [AtomicU32; BOARD_ROWS],
    show: AtomicU32,
    menu_len: AtomicU32,
    /// マッチ中の通信の状態の行を右下に置く（`NetUi::hud`が`status_side`と自分のサイドから決める。観戦者は常に左下）。
    hud_right: AtomicBool,
}

pub static BOARD: Board = Board::new();

impl Board {
    pub const fn new() -> Board {
        Board {
            rows: [const { TextBox::new() }; BOARD_ROWS],
            styles: [const { AtomicU32::new(0) }; BOARD_ROWS],
            show: AtomicU32::new(0),
            menu_len: AtomicU32::new(0),
            hud_right: AtomicBool::new(false),
        }
    }

    pub fn set_row(&self, i: usize, text: &str, style: RowStyle) {
        if let (Some(r), Some(s)) = (self.rows.get(i), self.styles.get(i)) {
            r.publish(text);
            s.store(style.code(), Ordering::Release);
        }
    }

    pub fn set_show(&self, bits: u32) {
        self.show.store(bits, Ordering::Release);
    }

    pub fn show(&self) -> u32 {
        self.show.load(Ordering::Acquire)
    }

    pub fn set_hud_right(&self, right: bool) {
        self.hud_right.store(right, Ordering::Release);
    }

    pub fn hud_right(&self) -> bool {
        self.hud_right.load(Ordering::Acquire)
    }

    pub fn set_menu_len(&self, n: usize) {
        self.menu_len.store(n as u32, Ordering::Release);
    }

    /// 行`i`をUTF-8で`out`へ読む。書きかけか範囲の外なら`None`（読み直しても駄目ならその行は描かない）。確保しない。
    fn read_row<'a>(
        &self,
        i: usize,
        out: &'a mut [u8; MAX_TEXT_UTF8],
    ) -> Option<(&'a str, RowStyle)> {
        let r = self.rows.get(i)?;
        let mut units = [0u16; TEXT_UNITS];
        let len = (0..4).find_map(|_| r.read(&mut units))?;
        let n = utf16_to_utf8(units.get(..len).unwrap_or(&[]), out);
        let text = core::str::from_utf8(out.get(..n).unwrap_or(&[])).unwrap_or("");
        let style = RowStyle::from_code(self.styles.get(i)?.load(Ordering::Acquire));
        Some((text, style))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Band {
    /// 上の枠の左（マッチの外の「待機中」の行）。
    TopLeft,
    /// 下の枠の左（マッチ中の通信の状態の行）。
    BottomLeft,
    /// 下の枠の右（`status_side`で右に置いたマッチ中の通信の状態の行。右寄せ）。
    BottomRight,
    /// 上の枠の左（試合中の1Pの名前）・右（2Pの名前。右寄せ）。
    NameLeft,
    NameRight,
}

/// 盤を描く頂点を作り、1まとまりずつ`emit`へ渡す（呼び手が1まとまりごとに`DrawPrimitiveUP`する）。戻り値はまとまりの数。
/// 見せるものが無ければ0を返す（何も読まない）。確保しない（頂点の配列と行のコピーはスタック）・パニックしない。
pub fn draw_board(
    board: &Board,
    spec: &AtlasSpec,
    screen_w: u32,
    screen_h: u32,
    emit: &mut dyn FnMut(&[Vertex]),
) -> usize {
    let show = board.show();
    if show == 0 {
        return 0;
    }
    let mut verts = [Vertex::default(); higanr_core::overlay::MAX_VERTS];
    let mut batches = 0usize;
    let row_h = lay::row_height(spec);
    if show & SHOW_MENU != 0 {
        let n = (board.menu_len.load(Ordering::Acquire) as usize).min(MENU_ROWS);
        let mut bufs = [[0u8; MAX_TEXT_UTF8]; MENU_ROWS];
        let mut rows: [Option<(usize, usize, RowStyle)>; MENU_ROWS] = [None; MENU_ROWS];
        let mut widest = 0u32;
        // 行の上端（枠の文字の置き場から）。隙間の行は行の高さより低いので前の行の高さを足していく。読み取れない行は1行分とみなす。
        let mut tops = [0u32; MENU_ROWS];
        let mut total = 0u32;
        for (i, buf) in bufs.iter_mut().enumerate().take(n) {
            tops[i] = total;
            let mut height = row_h;
            if let Some((text, style)) = board.read_row(i, buf) {
                widest = widest.max(lay::text_width(spec, text));
                rows[i] = Some((text.len(), i, style));
                height = style.height(row_h);
            }
            total = total.saturating_add(height);
        }
        let b = lay::menu_box(screen_w, screen_h, total, widest);
        let k = lay::push_menu_back(&mut verts, spec, screen_w, screen_h, &b);
        if k > 0 {
            emit(&verts[..k]);
            batches += 1;
        }
        for (i, row) in rows.iter().enumerate().take(n) {
            let Some((len, _, style)) = *row else {
                continue;
            };
            let Some(text) = bufs
                .get(i)
                .and_then(|bf| bf.get(..len))
                .and_then(|bytes| core::str::from_utf8(bytes).ok())
            else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            let y = b.text_y + tops[i] + lay::PAD;
            let (at, _) = lay::push_text(
                &mut verts,
                0,
                spec,
                (b.text_x, y),
                text,
                style.color(),
                b.text_w,
            );
            if at > 0 {
                emit(&verts[..at]);
                batches += 1;
            }
        }
    }
    let mut buf = [0u8; MAX_TEXT_UTF8];
    // 左上の行は出ているものを上から詰める（相手を待つ知らせは左上の行の続き）。
    let mut next_line = 0u32;
    for (bit, row) in TL_ROWS.into_iter().chain([
        (SHOW_PEER_WAIT, ROW_PEER_WAIT),
        (SHOW_COPY, ROW_COPY),
        (SHOW_UPD0, ROW_UPD0),
        (SHOW_UPD1, ROW_UPD1),
    ]) {
        if show & bit == 0 {
            continue;
        }
        let line = next_line;
        next_line += 1;
        let Some((text, style)) = board.read_row(row, &mut buf) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let max_w = screen_w.saturating_sub(2 * TL_X);
        let w = lay::text_width(spec, text).min(max_w);
        let y = TL_Y + line * row_h;
        let back = lay::push_rect(
            &mut verts,
            0,
            spec,
            (TL_X - lay::PAD, y - lay::PAD, w + 2 * lay::PAD, row_h),
            lay::COLOR_LINE_BACK,
        );
        let (at, _) = lay::push_text(
            &mut verts,
            back,
            spec,
            (TL_X, y),
            text,
            style.color(),
            max_w,
        );
        if at > 0 {
            emit(&verts[..at]);
            batches += 1;
        }
    }
    // 上の枠・下の枠の帯（下地の半透明の黒 ＋ GDIの文字。1まとまり）。
    // 文字はゲームの文字でなくGDIの文字（半角7画素。`push_text_plain`）。
    // ゲームの文字の9画素送りでは2つ空けの形が収まらない。下地の幅も同じ数え方（`text_width_plain`）。
    // 文字は中央のタイマー・fpsの手前までしか描かない（行の文字が長くなっても重ならずに後ろが切れる）。
    //
    // - 待っている間の行（`ROW_WAIT`）: 上の枠の左。試合中の帯が出ている間は出さない（同じ場所に名前の帯が来て重なるため）。
    //   待ちと接続は同時にならないが、重なって読めなくなるよりは試合中の帯を優先する。
    // - マッチ中の行（`ROW_HUD`。通信の状態）: 下の枠の左（`status_side`で右にしたときは右寄せで右。観戦者は左）。
    // - 名前の帯（`ROW_NAME_L` / `ROW_NAME_R`）: 上の枠の左に1P、右に2P（右寄せ）。サイドの語だけサイドの色。
    let hud_band = if board.hud_right() {
        Band::BottomRight
    } else {
        Band::BottomLeft
    };
    for (bit, row, band) in [
        (SHOW_WAIT, ROW_WAIT, Band::TopLeft),
        (SHOW_HUD, ROW_HUD, hud_band),
        (SHOW_NAMES, ROW_NAME_L, Band::NameLeft),
        (SHOW_NAMES, ROW_NAME_R, Band::NameRight),
    ] {
        if show & bit == 0 || (bit == SHOW_WAIT && show & SHOW_MATCH != 0) {
            continue;
        }
        let Some((text, style)) = board.read_row(row, &mut buf) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        // 置き場: 文字の左端`edge_x`・帯の上端`y`・文字を描ける幅の上限`cap`。
        let (edge_x, y, cap) = match band {
            Band::TopLeft => (HUD_X, 0, HUD_TEXT_MAX_W),
            Band::BottomLeft | Band::BottomRight => (
                lay::HUD_EDGE_X,
                lay::hud_bottom_y(screen_h),
                lay::hud_bottom_cap(screen_w, band == Band::BottomRight),
            ),
            Band::NameLeft | Band::NameRight => (lay::HUD_EDGE_X, 0, lay::HUD_NAME_MAX_W),
        };
        let max_w = cap.min(screen_w.saturating_sub(2 * edge_x));
        // 小さい文字の行（観戦者の左下の行）は文字の間隔と枠の描き方が違う（`RowStyle::Small`）。
        let small = style == RowStyle::Small;
        let w = if small {
            lay::text_width_small(spec, text)
        } else {
            lay::text_width_plain(spec, text)
        }
        .min(max_w);
        let x = if matches!(band, Band::NameRight | Band::BottomRight) {
            lay::hud_right_text_x(screen_w, w)
        } else {
            edge_x
        };
        // 名前の帯はサイドの語（「1P」「2P」）だけサイドの色。名前は行の色のまま。
        let accent = match band {
            Band::NameLeft => name_band_label_range(Seat::P1, text).map(|r| (r, SEAT_COLOR_1P)),
            Band::NameRight => name_band_label_range(Seat::P2, text).map(|r| (r, SEAT_COLOR_2P)),
            _ => None,
        };
        let back = lay::push_rect(
            &mut verts,
            0,
            spec,
            (x.saturating_sub(lay::PAD), y, w + 2 * lay::PAD, HUD_BAND_H),
            lay::COLOR_LINE_BACK,
        );
        let at = if small {
            lay::push_text_small(
                &mut verts,
                back,
                spec,
                (x, lay::small_text_y(spec, y)),
                text,
                style.color(),
                max_w,
            )
            .0
        } else {
            push_banded(
                &mut verts,
                back,
                spec,
                (x, y + HUD_Y),
                text,
                (style.color(), accent),
                max_w,
            )
        };
        if at > 0 {
            emit(&verts[..at]);
            batches += 1;
        }
    }
    batches
}

/// 名前の帯のサイドの語の色（マッチ終了時のメニューの縁取りと同じ色。不透明）。
const SEAT_COLOR_1P: u32 = 0xFF00_0000 | higanr_core::end_outline::RGB_1P;
const SEAT_COLOR_2P: u32 = 0xFF00_0000 | higanr_core::end_outline::RGB_2P;

/// 帯の文字`text`を左端`x`・枠の上端`y`から`out[at..]`へ書く（`push_text_plain`）。
/// `accent`のバイトの範囲だけ別の色で書く。範囲が文字の境目でなければ全部ふつうの色で書く。
/// 幅が`max_w`を越える文字は落とす（範囲をまたいで続けて数える）。戻り値は次の位置。
fn push_banded(
    out: &mut [Vertex],
    at: usize,
    spec: &AtlasSpec,
    (x, y): (u32, u32),
    text: &str,
    (color, accent): (u32, Option<(core::ops::Range<usize>, u32)>),
    max_w: u32,
) -> usize {
    let (lo, hi, accent_color) = match accent {
        Some((r, c))
            if r.start <= r.end
                && text.is_char_boundary(r.start)
                && text.is_char_boundary(r.end) =>
        {
            (r.start, r.end, c)
        }
        _ => (0, 0, color),
    };
    let mut at = at;
    let mut used = 0u32;
    for (part, c) in [
        (text.get(..lo), color),
        (text.get(lo..hi), accent_color),
        (text.get(hi..), color),
    ] {
        let Some(part) = part.filter(|p| !p.is_empty()) else {
            continue;
        };
        let (next, w) = lay::push_text_plain(
            out,
            at,
            spec,
            (x.saturating_add(used), y),
            part,
            c,
            max_w.saturating_sub(used),
        );
        at = next;
        used = used.saturating_add(w);
        if w < lay::text_width_plain(spec, part) {
            break;
        }
    }
    at
}

// ---- ゲームのスレッドの側 ----

/// ネットワーク（受信ソケット・STUN・監視スレッド）の状態。`code_side = game`ではメニューの最初の操作（作る / 貼る）で初めて開く。
/// 「妖怪 対 妖怪」が選ばれない限りゲームの動作に何も影響しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetPhase {
    /// まだ開いていない。
    Closed,
    /// 開いている最中（別のスレッドが受信ソケットを開き、STUNを問い合わせ、監視スレッドを開始する）。
    Opening,
    /// 開いた（ネットワークのセッションを登録した）。
    Open,
}

#[derive(Debug, Clone, Copy)]
pub struct UiFacts {
    pub conn: Conn,
    /// 自分の入力ワードを読むサイド（接続していれば自分のサイド。まだなら1P。台本の入力ワードの取り方がサイドで変わる）。
    pub seat: Seat,
    /// 今のUNIX秒（待った時間と有効期限の残り）。
    pub now: u64,
    pub net: NetPhase,
    /// 左上に警告を出すための材料。
    /// 接続の手順が時間切れで作り直されているとき（または相手のコードを読んで両方が直接接続できないネットワーク種別と分かったとき）に`net_input`が作る（`NetPlay::restarts`・`Net::own_nat`・`NetPlay::peer_nat`から）。
    /// あれば左上を小さい1行でなくゲームの文字の警告（最大5行。`net_menu::stall_block`）にする。ネットワークを開いていない実行は`None`。
    pub stall: Option<Stall>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HudFacts {
    /// 入力の遅れ（rollbackなら今のD。delay方式なら今の遅れ）。
    /// どちらも、両者がCtrl＋数字で変えて決まった値か要求している値があればその値（`NetPlay::shown_delay`）。
    pub latency: Option<u8>,
    pub rtt_ms: Option<u32>,
    /// rollbackのときは前のtickから今までに進め直した更新の数（`Rollback::stats`の差）。delay方式なら`None`。
    pub rewound: Option<u32>,
    /// 遅れの推奨値（`Connected::recommend_now`。RTTが測れていなければ`None`）。
    pub recommend: Option<u8>,
    /// autoのフラグ（ホストは自分のフラグ。ゲストはホストのフラグか自分のautoの要求 —— `Connected::shown_auto`）。立っていれば行の括弧の中は`A`。
    pub auto: bool,
    /// キャラセレクト（画面8）の行のとき先頭に出す対戦の形式（`NetPlay::hud_facts`が選択の局面で`Some`にする）。マッチ中は`None`。
    pub format: Option<Mode>,
    /// キャラセレクトの行で、この組み合わせはrollbackへ替えられない（片方がrollbackできない）。
    /// `format`がdelay方式のときは行に「delay only」と出る（`net_menu::HudView::delay_only`）。
    pub delay_only: bool,
    /// 試合中の両者の名前（HELLOで交換した名前をサイドに合わせて並べたもの）。接続していなければ`None`。
    pub names: Option<HudNames>,
    /// 自分のサイド（接続していれば`Some`。通信の状態の行の置き場所`status_side`が使う）。
    pub seat: Option<Seat>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WatchHudFacts {
    /// 観戦しているマッチの1P・2Pの名前（観戦の流れの`Name`の記録を両サイド分受けるまでは`None`。その間は名前の帯だけ出さない）。
    pub names: Option<HudNames>,
    /// ホストが配信した対戦する2人の間の状態（観戦の流れの`Stats`の記録の最新の値。まだ受けていなければ`None`）。
    pub stats: Option<PeerStats>,
    /// 使う直前に溜まっていたマッチのフレームの数（直近の期間の最大）。
    pub delay_frames: u32,
    /// 早送りの最中。
    pub fast_forward: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HudNames {
    pub p1: Name,
    pub p2: Name,
}

impl HudNames {
    pub fn of(seat: Seat, own: Name, peer: Name) -> HudNames {
        match seat {
            Seat::P1 => HudNames { p1: own, p2: peer },
            Seat::P2 => HudNames { p1: peer, p2: own },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UiOut {
    /// `Some`ならこのUIがゲームの入力を占有してfeedを済ませた（呼び手はそのactionを返す）。`None`ならゲームの入力に触っていない。
    pub action: Option<Action>,
    /// ネットワークを開く頼み（メニューの最初の「作る」「貼る」。呼び手が別のスレッドで開く。2回目からは出ない）。
    pub open_net: bool,
    /// 確認の枠で「はい」と答えた。呼び手が対戦を終える（BYE。抜ける側の取消）。
    pub leave: bool,
    /// 自分が「妖怪 対 妖怪」を選んだ（接続した後・成立の前に選んだか、接続した瞬間にMatchの画面でメニューを開いていた）。
    /// 呼び手がREADYを送る（`NetPlay::set_ready`。`higanr_core::match_gate`）。
    pub ready: bool,
    /// 観戦の「ルームマッチ」が決定した（このtickに`Cmd::Spectate`を監視スレッドへ送った）。
    /// 呼び手が観戦者のネットワークを開く仕事を頼む（`script_win::request_spectate`）。
    /// 対戦する側のネットワークは開かない（`open_net`は出ない。インターネット越しの観戦v1）。
    pub spectate_open: bool,
    /// 戻り道（`return_flow`）が終わりのメニューの「リプレイを保存して終了」で試合の場面を出るのを見た。
    /// 呼び手が保存の画面を飛ばす印を立てる（`save_skip`）。
    pub save_quit: bool,
    /// LANの項目が決定した（このtickに`Cmd::LanHost` / `Cmd::LanClient` / `Cmd::LanWatch`が出た）。
    /// 呼び手がLANの運び手のスレッドを作る（`script_win::request_lan`）。対戦する側のネットワークはここでは開かない（`open_net`は出ない）。
    pub lan: Option<LanAsk>,
}

/// LANの項目で頼まれたこと。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanAsk {
    /// ホストとして待つ。
    Host,
    /// 打ったアドレスのホストへ対戦する側として参加を申し込む。
    Client(LanAddr),
    /// 打ったアドレスのホストへ観戦を申し込む。
    Watch(LanAddr),
}

#[derive(Clone, Copy, Default)]
struct Shown {
    line: LineBuf,
    style: RowStyle,
}

/// 自動の前進（接続した後にMatchの画面を「人間 対 人間」まで進める）が押し・待つのに使えるtickの上限（10秒。60 fps）。
/// 上へ動かすのは最大4回と決定1回なので10 tickほどで足りる。
/// カーソルが動かないなどで進まないまま押し続けるのを止める歯止め。超えたら前進をやめて人の入力ワードに戻る。
/// 前進は接続したときMatchの画面にいた側だけが行う（画面を出たら終わり）。後からMatchの画面へ来た側は動かさない。
/// `code_side = game`の「つながったとき」は「成立したとき」を指す（両者が「妖怪 対 妖怪」を選んだとき。`Conn::Connected`になったtick）。
/// 「妖怪 対 妖怪」を選んだ側は成立までMatchの画面で待つ（`tick`の4'）ので成立した瞬間は両者ともMatchの画面にいる。
const AUTO_MAX_TICKS: u32 = 600;

/// 取消の入力ワード（X。確認の枠で「はい」と答えた後にゲームへ1 tickだけ通す）。
const KEY_CANCEL: u16 = keys::X;

/// 観戦コードのコピーの結果を左上に出す秒数（UNIX秒の単位なので4〜5秒）。
const COPY_NOTICE_SECS: u64 = 5;

/// Ctrl+Cを頼んでから結果のイベントを待つ上限（秒）。監視スレッドは50 msごとに指示を取るので普段は1秒もかからない。
/// 超えたら（監視スレッドがまだ接続を見ていない等）結果を待つのをやめる。
const COPY_WAIT_SECS: u64 = 10;

/// LANのアドレスの入力が使う仕事。自分のPCのVPNのネットワークのスナップショットを扱う（製品は`vpn_snap`）。
#[derive(Clone, Copy)]
pub struct VpnHooks {
    /// スナップショットの更新を頼む（別のスレッドを作って返る）。
    pub scan: fn(),
    /// スナップショットを読む（確保しない）。
    pub read: fn() -> VpnSet,
}

/// 新しいバージョンの確認の仕事（本物は`update_check`。確保しない・待たない）。
#[derive(Clone, Copy)]
pub struct UpdateHooks {
    /// 確認を頼む（別のスレッドが取る）。
    pub ask: fn(),
    /// 見つかった新しいバージョン（`Found::to_raw`。0は見つかっていない）。
    pub found: fn() -> u32,
}

/// `update_tick`が前のtickに見た状態（きっかけは立ち上がりだけで拾う）。
#[derive(Debug, Clone, Copy, Default)]
struct UpdateSeen {
    menu_open: bool,
    linked: bool,
    end_menu: bool,
}

pub struct NetUi {
    menu: Menu,
    link: Arc<UiLink>,
    board: &'static Board,
    note: Note,
    shown: [Shown; BOARD_ROWS],
    shown_mask: u32,
    shown_menu_len: usize,
    rewind: RewindWindow,
    /// 成立した後の自動の前進（`Some(経った tick)`）。
    auto: Option<u32>,
    /// 前のtickに成立していた。成立した瞬間に自動の前進を始める。
    was_connected: bool,
    /// 前のtickに「つながったが成立の前」（`Conn::Found`）だった。接続した瞬間を見る（その瞬間にメニューを開いていた側は
    /// 選んだものとする）。
    was_found: bool,
    prev_word: u16,
    /// ネットワーク一式を後始末した（`after_teardown`）ときの終わりの状態。ネットワークを閉じている間は左上の行にこれを出し続ける。
    /// 入口のメニューを開くか次のネットワークを開き始めたら消す。
    ended: Option<Conn>,
    /// 後始末したとき、監視スレッドの状態のタグ（`Note::seq`）がこの値のままなら読まない。
    /// 前の監視スレッドの状態をワーカースレッドが消すまでの数msに読み戻さないため。
    note_hold: Option<u32>,
    /// 観戦者のセッションが設定されていてホストの流れを受け始めた（`WatchPlay`が`Selecting`）。
    /// そのため画面6で自分のゲームを画面7まで自動で進める（`set_watch_advance`）。
    /// 接続の状態は`Conn::Waiting`のまま（`Conn::Connected`にすると左上に「つながりました。あなたは 1P です」が出て取消が対戦の確認の枠になる）。
    watch_advance: bool,
    /// メニューの場面で最後に読んだ画面の番号（`tick_inner`が毎tick書く）。
    /// 左上の「つながりました」の行を画面7（難易度）までだけ出すための目印（`publish_corner`）。
    menu_screen: Option<u32>,
    /// ウィンドウから文字を貯めるゲートが開いている（ルーム名の入力の間だけ）。
    text_gate: bool,
    /// いまの入力でゲートを開けようとしたかどうか（開けられたかは`text_gate`。入力を出るとfalseに戻る）。
    text_gate_tried: bool,
    hud_names: Option<HudNames>,
    /// マッチ中の通信の状態の行の置き場所の設定（iniの`[ui] status_side`）。
    status_side: StatusSide,
    /// 盤へ書いた置き場所（右下か）。変わったときだけ書く。
    hud_right: bool,
    /// 観戦者の知らせの行に今出している知らせ（`watch_notice`）。
    watch_notice_shown: Option<WatchNotice>,
    /// プレイヤー名を決定したとき（`Cmd::Name`）に呼ぶiniの`name`へ書く仕事（`ini_save::save_name`）。
    /// ゲームのスレッドでファイルに書かないため、別のスレッドを作って書かせる。設定しなければ（検査）書かない。
    name_saver: Option<fn(NameBuf)>,
    /// 設定の「ルーム名」を決定したとき（`Cmd::RoomSet`）に呼ぶiniの`room`へ書く仕事（`ini_save::save_room`。
    /// `name_saver`と同じ作り）。設定しなければ（検査）書かない。
    room_saver: Option<fn(RoomBuf)>,
    /// 言語を替えたとき（`Cmd::Lang`）に呼ぶ仕事。今の言語を設定してiniへ書く（`script_win::save_ui_lang`）。設定しなければ（検査）書かない。
    lang_saver: Option<fn(Lang)>,
    /// LANのホストのアドレスを決定したとき（`Cmd::LanClient` / `Cmd::LanWatch`）に呼ぶiniの`lan_host`へ書く仕事（`ini_save::save_lan_host`）。
    /// 設定しなければ（検査）書かない。
    lan_saver: Option<fn(LanAddr)>,
    /// 自分のPCのVPNのネットワークのスナップショットを更新させる仕事（LANのアドレスの入力に入ったときに呼ぶ。別のスレッドを作る）と、スナップショットの読み取り（`vpn_snap`）。
    /// 設定しなければ（検査）VPNのアドレスは受けない。
    vpn_hooks: Option<VpnHooks>,
    /// 前のtickにLANのアドレスの入力の間だった（入った瞬間にスナップショットの更新を頼む）。
    vpn_editing: bool,
    /// Ctrl+C（観戦コードのコピー）の押した瞬間の取り方。
    /// 数字・Tabの取り方（`NetPlay`の持つもの）とは別に持つ。二重に`feed`しても取り合わないようにするため。
    copy_edge: DigitEdge,
    /// 対戦のホストへの案内（「Ctrl+C で観戦コードをコピーできます。」）を出す実行かどうか（iniの`[spectate] enable = 1`）。
    copy_hint_on: bool,
    /// コピーを頼んだ後に結果のイベントを待っている（頼んだ時点の`Note::seq`と待つのをやめるUNIX秒）。
    copy_wait: Option<(u32, u64)>,
    /// 左上に出している結果のイベントと消すUNIX秒。
    copy_shown: Option<(Event, u64)>,
    /// 同期方式かディレイを替えたとき（`Cmd::Shape`）に呼ぶiniの`mode`と`delay`へ書く仕事（`ini_save::save_shape`）。設定しなければ（検査）書かない。
    shape_saver: Option<fn(Pick, DelayChoice)>,
    /// サイドを替えたとき（`Cmd::Seat`）に呼ぶiniの`host_seat`へ書く仕事（`ini_save::save_seat`）。設定しなければ（検査）書かない。
    seat_saver: Option<fn(SeatWish)>,
    /// 観戦の「ランダムマッチ」の一覧の画面に入った（`true`）・出た（`false`）ときに呼ぶ、一覧の接続を開け閉めする仕事（`script_win`）。
    /// 開ける仕事はスレッドを作る。メニューの外では呼ばない。設定しなければ（検査）一覧は届かない。
    watch_feed: Option<fn(bool)>,
    /// いま一覧の画面で一覧の接続を開けた。
    watch_on: bool,
    /// メニューが読む一覧。読み込み用の`watch_scratch`で書きかけでないことを検証してから中身を入れ替える（確保しない。どちらも作るときに1回だけ確保）。
    watch: Box<WatchSnapshot>,
    watch_scratch: Box<WatchSnapshot>,
    /// 読んだ一覧のバージョン（`UiLink::read_watch_list`）。
    watch_seen: u32,
    /// 対戦の後に接続が終わったら「妖怪 対 妖怪」へ戻す（`with_return_flow`。設定しない実行・検査は今までどおり）。
    ret_on: bool,
    /// 戻している途中。
    ret: Option<Flow>,
    /// 戻した理由（左上の1行目）。入口のメニューを開く・次の相手と接続するまで出す。
    lost: Option<Why>,
    /// 戻り始めに鳴らす音（別のスレッドを作って鳴らす仕事。`script_win`）。
    lost_chime: Option<fn()>,
    /// 確認の枠で「抜ける」と答えた（そのBYE(取消)では戻さない。ゲームが取消で自分で画面6へ戻る）。
    left_by_menu: bool,
    /// 戻り道のカウント（始めた・着いた・やめた）が変わるたびに呼ぶ。本ログへ出すアトミック変数へコピーする仕事（`script_win`）。設定しなければ（検査）呼ばない。
    ret_log: Option<fn(&UiStats)>,
    stats: UiStats,
    /// 新しいバージョンの確認（設定しない実行・検査は確認しない）。
    update_hooks: Option<UpdateHooks>,
    update_poll: Poll,
    update_found: Found,
    update_seen: UpdateSeen,
    /// ローカルの人間 対 人間でキーボード2台を分けるステートマシン・iniの設定・`tick_inner`が最後に見た場面。
    split: split_kb::Machine,
    split_on: bool,
    scene: split_kb::Scene,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UiStats {
    pub opened: u32,
    pub intercepts: u32,
    pub held_ticks: u32,
    pub orders_sent: u32,
    pub auto_presses: u32,
    /// 自動の前進を始めた回数（成立した瞬間に1回。重複・遅れたREADYで2度始めないことを検査が見る）。
    pub auto_starts: u32,
    /// 確認の枠を開いた回数と「はい」と答えた回数。
    pub confirms: u32,
    pub leaves: u32,
    /// 「妖怪 対 妖怪」を選んだ（READYを頼んだ）回数（接続した瞬間にメニューを開いていた分を含む）。
    pub readies: u32,
    /// ネットワーク一式を後始末してメニューを「つなぐ前」へ戻した回数（再接続。`after_teardown`）。
    pub teardowns: u32,
    /// Ctrl+Cで観戦コードのコピーを頼んだ回数（押した瞬間に1回。押しっぱなしでは増えない）。
    pub copy_asks: u32,
    /// 「妖怪 対 妖怪」へ戻し始めた回数・着いた回数・途中でやめた回数（`return_flow::Finish`）。
    pub returns: u32,
    pub return_arrived: u32,
    pub return_gave_up: u32,
    /// 最後に途中でやめた理由（`GiveUp::encode`。0はまだ無い）。
    pub return_why: u32,
}

impl NetUi {
    pub fn new(ini: Shape, link: Arc<UiLink>, board: &'static Board) -> NetUi {
        NetUi {
            menu: Menu::new(ini),
            link,
            board,
            note: Note::default(),
            shown: [Shown::default(); BOARD_ROWS],
            shown_mask: 0,
            shown_menu_len: 0,
            rewind: RewindWindow::new(),
            auto: None,
            was_connected: false,
            was_found: false,
            prev_word: 0,
            ended: None,
            note_hold: None,
            watch_advance: false,
            menu_screen: None,
            text_gate: false,
            text_gate_tried: false,
            hud_names: None,
            status_side: StatusSide::default(),
            hud_right: false,
            watch_notice_shown: None,
            name_saver: None,
            room_saver: None,
            lang_saver: None,
            lan_saver: None,
            vpn_hooks: None,
            vpn_editing: false,
            copy_edge: DigitEdge::new(),
            copy_hint_on: false,
            copy_wait: None,
            copy_shown: None,
            shape_saver: None,
            seat_saver: None,
            watch_feed: None,
            watch_on: false,
            watch: Box::new(WatchSnapshot::EMPTY),
            watch_scratch: Box::new(WatchSnapshot::EMPTY),
            watch_seen: 0,
            ret_on: false,
            ret: None,
            lost: None,
            lost_chime: None,
            left_by_menu: false,
            ret_log: None,
            stats: UiStats::default(),
            update_hooks: None,
            update_poll: Poll::new(0),
            update_found: Found::None,
            update_seen: UpdateSeen::default(),
            split: split_kb::Machine::new(),
            split_on: false,
            scene: split_kb::Scene::Other,
        }
    }

    /// 新しいバージョンの確認の仕事を設定する。`build_number`が0（開発用のビルド）なら確認の依頼は出さない。
    pub fn with_update_check(mut self, hooks: Option<UpdateHooks>, build_number: u32) -> NetUi {
        self.update_hooks = hooks;
        self.update_poll = Poll::new(build_number);
        self
    }

    /// LANのアドレスの入力の間だけ自分のPCのVPNのネットワークをメニューへ渡す。入ったtickにスナップショットの更新を頼む（別のスレッド）。
    /// 入力に何秒もかかるので打ち終わるまでに更新は済む。入力の間でなければ何もしない。
    fn feed_vpn_nets(&mut self) {
        let editing = self.menu.wants_lan_addr();
        if let (true, Some(h)) = (editing, self.vpn_hooks) {
            if !self.vpn_editing {
                (h.scan)();
            }
            self.menu.set_vpn((h.read)());
        }
        self.vpn_editing = editing;
    }

    /// 観戦の「ランダムマッチ」の一覧の接続を開け閉めする仕事を設定する。
    pub fn with_watch_feed(mut self, feed: Option<fn(bool)>) -> NetUi {
        self.watch_feed = feed;
        self
    }

    /// ホストの案内を出す実行にする（観戦のホスト`[spectate] enable = 1`のとき）。
    pub fn with_spectate_hint(mut self, on: bool) -> NetUi {
        self.copy_hint_on = on;
        self
    }

    pub fn with_start_auto(mut self, auto: bool) -> NetUi {
        self.menu = self.menu.with_start_auto(auto);
        self
    }

    pub fn with_seat(mut self, wish: SeatWish) -> NetUi {
        self.menu = self.menu.with_seat(wish);
        self
    }

    /// 「名前」の項目を追加する（`name`は今のプレイヤー名。ゲーム内のメニューの実行だけ）。
    /// `saver`は決定した名前をiniへ書く仕事（ゲームのスレッドでは書かない）。
    pub fn with_player_name(mut self, name: NameBuf, saver: Option<fn(NameBuf)>) -> NetUi {
        self.menu = self.menu.with_player_name(name);
        self.name_saver = saver;
        self
    }

    pub fn with_room_saver(mut self, saver: Option<fn(RoomBuf)>) -> NetUi {
        self.room_saver = saver;
        self
    }

    /// LANの「クライアント」「観戦」のアドレスの入力の初めの値（前回打ったアドレス）と、決定したときにiniへ書く仕事。
    pub fn with_lan_addr(mut self, addr: AddrBuf, saver: Option<fn(LanAddr)>) -> NetUi {
        self.menu = self.menu.with_lan_addr(addr);
        self.lan_saver = saver;
        self
    }

    /// LANのアドレスの入力がVPNのネットワークを取得する仕事を設定する。
    pub fn with_vpn_hooks(mut self, hooks: Option<VpnHooks>) -> NetUi {
        self.vpn_hooks = hooks;
        self
    }

    /// 「同期方式」「ディレイ」を替えたときに呼ぶiniへ書く仕事。
    pub fn with_shape_saver(mut self, saver: Option<fn(Pick, DelayChoice)>) -> NetUi {
        self.shape_saver = saver;
        self
    }

    /// マッチ中の通信の状態の行を左右どちらに置くか。
    pub fn with_status_side(mut self, side: StatusSide) -> NetUi {
        self.status_side = side;
        self
    }

    /// ローカルの人間 対 人間でキーボード2台を1P・2Pに分けるか（iniの`[input] split_keyboards`）。
    pub fn with_split_keyboards(mut self, on: bool) -> NetUi {
        self.split_on = on;
        self
    }

    pub fn split_on(&self) -> bool {
        self.split_on
    }

    /// キーボード2台の分け方を1 tick進める（メニューが入力を占有しないtickに`script_win::ui_only_tick`が呼ぶ）。
    /// `Act::Pass`以外のときは、呼び手が`Hold`なら0を、`Feed`なら台ごとの入力ワードをfeedする。
    /// 左上の案内の行はここで出す。
    ///
    /// 遅延の増分: 台ごとの押下の表の読み（分けていない間は押された台の集合と記録の後始末だけ）。
    /// 聞く間と、使わないと決めて離すのを待つ間は素のpollの3本の読み。
    /// 分けている間は読まず、割り当てた2台の入力ワードをfeedする（ゲームが`poll_all`で読む分の代わりで増えない）。
    /// 分けている間は前面の問い合わせも加わる。
    pub fn split_step<G: Game>(
        &mut self,
        game: &mut G,
        table: &DeviceTable,
        net: bool,
    ) -> split_kb::Act {
        let scene = self.scene;
        if self.split_on && self.split.arming(scene) && !game.scripted_run() {
            // 聞き始める前にDirectInputがAcquireをやり直して登録のウィンドウを作り直していないかを1回調べる。
            crate::split_kb_win::refresh();
        }
        let mut f = split_kb::Facts {
            enabled: self.split_on,
            net,
            scene,
            snap: table.snap(self.split.wants_full(scene)),
            ..split_kb::Facts::default()
        };
        if self.split.polls() && !game.scripted_run() {
            let [a, b, c] = game.menu_words(Seat::P1);
            f.game_word = a | b | c;
        }
        if self.split.is_active() {
            f.gate = game.keys_gate();
        }
        if scene == split_kb::Scene::Match && self.split.is_engaged() {
            f.replay = game.replaying();
        }
        let act = self.split.step(&f);
        // イベントは詳細のログだけに1行ずつ（tickごとには起きない）。
        if let Some(ev) = self.split.take_event() {
            crate::split_kb_win::log_event(ev);
        }
        if let split_kb::Act::Hold(Some(prompt)) = act {
            let (n, lines) = higanr_core::net_menu::split_ask_lines(prompt, self.menu.lang());
            let mut mask = self.shown_mask & !(SHOW_TL0 | SHOW_TL1);
            for ((row, bit), (text, style)) in [(ROW_TL0, SHOW_TL0), (ROW_TL1, SHOW_TL1)]
                .into_iter()
                .zip(lines)
                .take(n)
            {
                self.put(row, text, style);
                mask |= bit;
            }
            self.set_mask(mask);
        }
        act
    }

    #[cfg(test)]
    pub fn split_phase(&self) -> split_kb::Phase {
        self.split.phase()
    }

    /// 「サイド」を替えたときに呼ぶiniへ書く仕事。
    pub fn with_seat_saver(mut self, saver: Option<fn(SeatWish)>) -> NetUi {
        self.seat_saver = saver;
        self
    }

    /// 画面の言語の初めの値と「言語」の行で替えたときの仕事。
    pub fn with_lang(mut self, lang: Lang, saver: Option<fn(Lang)>) -> NetUi {
        self.menu = self.menu.with_lang(lang);
        self.lang_saver = saver;
        self
    }

    /// iniの読み取れない行を捨てたことをメニューの下に出す。
    pub fn with_ini_notice(mut self, notice: Option<higanr_core::net_menu::IniNotice>) -> NetUi {
        self.menu = self.menu.with_ini_notice(notice);
        self
    }

    #[cfg(test)]
    pub(crate) fn lang(&self) -> Lang {
        self.menu.lang()
    }

    /// 観戦者のセッションが設定されていてホストの流れを受けている間true（`watch_input`が毎tick設定する）。
    /// 真の間は、接続した対戦する側と同じ自動の前進（画面6でカーソルを人間 対 人間まで上げて決定）がかかる。
    pub fn set_watch_advance(&mut self, on: bool) {
        self.watch_advance = on;
    }

    /// 観戦者のセッションを手放した（ゲームのスレッド。確保しない）: 自動の前進の印を消す。
    /// 前の監視スレッドの状態は消さない（観戦の終わりの理由を左上に残す）。
    /// 観戦者の状態は監視スレッド（`spectate_join`）が最後に書いたものが後始末の後も正しい。
    pub fn after_watch_teardown(&mut self) {
        self.watch_advance = false;
        self.auto = None;
        self.was_connected = false;
        self.was_found = false;
        self.ended = None;
        // 観戦のマッチ中の帯（名前・通信の状態）を残さない（セッションを手放した後は`watch_hud`を呼ぶ者がいない）。
        self.hud(false, HudFacts::default());
        self.watch_select_wait(false);
        self.copy_clear();
        self.stats.teardowns = self.stats.teardowns.saturating_add(1);
    }

    pub fn link(&self) -> &Arc<UiLink> {
        &self.link
    }

    /// ネットワーク一式を後始末した（ゲームのスレッド。`script_win`がネットワークのセッションを手放したtickに1回。確保しない）。
    /// メニューを「つなぐ前」へ戻す。
    /// `last`は後始末する直前の接続の状態（`NetPlay::ui_conn`）で、終わりの状態なら左上に出し続ける。
    ///
    /// メニューの開き具合（状態・カーソル・選択中の対戦の形）は変えない。
    /// 開いていれば次のtickから「作る」「貼る」の項目に戻る（開いたまま閉じると、押したままのキーをゲームが押した瞬間と読みうる）。
    /// 前の監視スレッドの状態は捨てて、ワーカースレッドが既定の状態を書くまで読み戻さない（`note_hold`）。
    /// 成立・自動の前進の印は次の接続へ持ち越さない。
    pub fn after_teardown(&mut self, last: Conn) {
        self.ended = last.is_end().then_some(last);
        self.note_hold = Some(self.note.seq);
        self.note = Note::default();
        self.auto = None;
        self.was_connected = false;
        self.was_found = false;
        self.peer_wait(false);
        self.copy_clear();
        // 戻り道（`ret`）と理由の行（`lost`）は後始末をまたいで続ける（後始末は終わりから2秒後で場面を問わない）。
        self.left_by_menu = false;
        self.stats.teardowns = self.stats.teardowns.saturating_add(1);
    }

    /// 観戦コードの知らせ（案内と結果）を消し、結果の待ちをやめる。
    fn copy_clear(&mut self) {
        self.copy_wait = None;
        self.copy_shown = None;
        if self.shown_mask & SHOW_COPY != 0 {
            self.set_mask(self.shown_mask & !SHOW_COPY);
        }
    }

    fn corner_conn(&self, f: &UiFacts) -> Conn {
        match (f.conn, self.ended) {
            (Conn::Waiting, Some(e)) => e,
            (c, _) => c,
        }
    }

    pub fn with_room(mut self, room: RoomItem) -> NetUi {
        self.menu = self.menu.with_room(room);
        self
    }

    pub fn stats(&self) -> UiStats {
        self.stats
    }

    #[cfg(test)]
    pub fn menu_active(&self) -> bool {
        self.menu.is_active()
    }

    fn feed3<G: Game>(game: &mut G, p1: u16, p2: u16) {
        game.feed(0, p1);
        game.feed(1, p2);
        game.feed(2, p1 | p2);
    }

    fn refresh_note(&mut self) {
        if let Some(n) = self.link.note() {
            if self.note_hold == Some(n.seq) {
                // 後始末した前の監視スレッドの状態のまま（ワーカースレッドがまだ消していない）。
                return;
            }
            self.note_hold = None;
            self.note = n;
        }
    }

    fn put(&mut self, row: usize, text: &str, style: RowStyle) {
        let same = self
            .shown
            .get(row)
            .is_some_and(|s| s.line.as_str() == text && s.style == style);
        if same {
            return;
        }
        self.board.set_row(row, text, style);
        if let Some(s) = self.shown.get_mut(row) {
            s.line.clear().push_str(text);
            s.style = style;
        }
    }

    fn set_mask(&mut self, mask: u32) {
        if mask != self.shown_mask {
            self.board.set_show(mask);
            self.shown_mask = mask;
        }
    }

    fn publish_menu(&mut self, f: &UiFacts) {
        let mut rows = MenuRows::new();
        self.menu
            .render_with(f.conn, &self.note, f.now, &mut rows, &self.watch);
        for i in 0..rows.len {
            let (text, style) = (rows.lines[i], rows.styles[i]);
            self.put(i, text.as_str(), style);
        }
        if rows.len != self.shown_menu_len {
            self.board.set_menu_len(rows.len);
            self.shown_menu_len = rows.len;
        }
        let hud = self.shown_mask & SHOW_KEPT;
        self.set_mask(SHOW_MENU | hud);
    }

    /// 「つながりました」の行を出してよい場面かどうか（メニューの場面の画面7まで）。
    fn connected_line_shows(&self) -> bool {
        self.menu_screen
            .is_some_and(|s| s <= u32::from(SCREEN_DIFFICULTY))
    }

    /// 左上の行を盤へ（メニューは閉じている）。
    ///
    /// 「つながりました」（`Conn::Connected`）は画面7（難易度）までだけ出す。画面8から先は名前の帯と左下の行が替わりに出る。
    /// 待ち・見つかった・閉じた理由などの他の行は画面によらず出す（別の知らせまで消さない）。
    fn publish_corner(&mut self, f: &UiFacts) {
        let conn = self.corner_conn(f);
        let mut lines = [LineBuf::new(); 2];
        let mut styles = [RowStyle::Normal; 2];
        let lang = self.menu.lang();
        // 接続できないときの警告は最大5行。出す場面では`corner_lines`は0を返す。
        let mut block = [LineBuf::new(); STALL_ROWS];
        let mut block_styles = [RowStyle::Normal; STALL_ROWS];
        let blocks = stall_block(
            conn,
            &self.note,
            f.now,
            f.stall,
            lang,
            &mut block,
            &mut block_styles,
        );
        let mut n = match self.lost {
            Some(why) if conn == Conn::Finished => return_lines(why, lang, &mut lines, &mut styles),
            _ => corner_lines(
                conn,
                &self.note,
                f.now,
                f.stall,
                lang,
                &mut lines,
                &mut styles,
            ),
        };
        if matches!(conn, Conn::Connected { .. }) && !self.connected_line_shows() {
            n = 0;
        }
        let mut mask = self.shown_mask & SHOW_KEPT;
        if blocks > 0 {
            for (i, (bit, row)) in TL_ROWS.into_iter().enumerate() {
                if i < blocks {
                    self.put(row, block[i].as_str(), block_styles[i]);
                    mask |= bit;
                }
            }
        } else if n > 0 && corner_is_band(conn, &self.note, f.stall) {
            // 待っている間の1行は左上の2行の置き場でなくマッチ中の行と同じ置き場へ置く（`net_menu::corner_is_band`）。
            self.put(ROW_WAIT, lines[0].as_str(), styles[0]);
            mask |= SHOW_WAIT;
        } else {
            for (i, (bit, row)) in TL_ROWS.into_iter().take(2).enumerate() {
                if i < n {
                    self.put(row, lines[i].as_str(), styles[i]);
                    mask |= bit;
                }
            }
        }
        self.set_mask(mask);
    }

    fn publish_state(&mut self, f: &UiFacts) {
        if self.menu.is_active() {
            self.publish_menu(f);
        } else {
            self.publish_corner(f);
        }
    }

    /// 場面がメニューでない（マッチ・読み込み）ときはメニューを閉じて左上の行を消す（マッチ中の行は`hud`が決める）。
    /// ただし待っている間の行（`Conn::Waiting`）・見つかった行（`Conn::Found`）・成立しないまま閉じた理由の行（`Conn::Unmatched`）・相手のバージョン違いの行（`Conn::OtherVersion`）は、どの場面でも出す（別のモードで遊んでいる側が左上の行と音で知れるように）。
    /// 位置は次のとおり。見つかった行と閉じた理由の行は上の枠の下の左上（`TL_Y`）。待っている間の1行はマッチ中の行と同じ上の枠の左（`ROW_WAIT`）。
    /// 成立した行・終わった行はマッチの中では出さない。
    /// 同期ズレでマッチを止めている間の2行（`Conn::Desynced`）はマッチの場面で出す唯一の終わりの行（止めたままEscを待つ間は画面に理由と次にすることを残す）。
    fn leave_menu_scene(&mut self, f: &UiFacts) {
        self.menu.reset();
        self.auto = None;
        let conn = self.corner_conn(f);
        if matches!(
            conn,
            Conn::Waiting
                | Conn::Found { .. }
                | Conn::Unmatched(_)
                | Conn::OtherVersion { .. }
                | Conn::OtherBuild { .. }
                | Conn::Desynced
        ) || (self.lost.is_some() && conn == Conn::Finished)
        {
            self.publish_corner(f);
        } else {
            let hud = self.shown_mask & SHOW_KEPT;
            self.set_mask(hud);
        }
    }

    fn choose(&mut self, out: &mut UiOut) {
        out.ready = true;
        self.stats.readies = self.stats.readies.saturating_add(1);
    }

    /// 0を3本feedしてこのtickを終える（メニュー・確認の枠・自動の前進のどれでも「何も押さない」）。
    fn hold<G: Game>(&mut self, game: &mut G, f: &UiFacts, mut out: UiOut) -> UiOut {
        Self::feed3(game, 0, 0);
        self.publish_state(f);
        out.action = Some(Action::Menu);
        out
    }

    /// 入力の地点の先頭で1 tickに1回呼ぶ。
    ///
    /// 画面6（Match）の間はメニューの有無によらずこのUIがゲームの入力を占有する（`Game::local_input`でワードを読み、`feed`で渡す）。
    /// これは「妖怪 対 妖怪」の決定を**ゲームが読む前に**ワードから判定するため。
    /// ゲーム自身のpollに任せると判定したワードとゲームが後で読むワードの間にキーが動きうる。
    /// ゲームへ漏れると判定したtickだけワードを0に替え、それ以外は変えずに渡す。
    /// 接続した後は画面7（難易度）の取消も監視する（確認の枠）。
    /// 画面7の取消でないtickは読んだワードを`stash_local`に登録して`None`を返す（ネットの選択が同じワードを使うので台本・デバイスを二重に読まない）。
    ///
    /// 対戦の成立（`Conn::Found`の間。`higanr_core::match_gate`）では、次の側が選択済み（`UiOut::ready`）になる。
    /// 接続した瞬間にMatchの画面で入口のメニューを開いていた側と、成立の前に「妖怪 対 妖怪」を選んだ側。
    /// 選択済みの側はMatchの画面で入力を占有して相手を待つ（キャンセルすると「対戦から抜けますか？」を出す）。
    /// 選択済みでない側の入力には触らない（別のモードで遊べる。画面7も占有しない）。
    ///
    /// ルーム名の入力の間はウィンドウから文字を貯めるゲートを開ける（`Game::text_gate`。どの経路でtickを終えても通る）。
    /// 貯まった文字は1 tickに1回まとめて取ってメニューへ渡す（`Game::typed`）。
    /// 入力の間もゲームへは0を3本feedする（メニューがゲームの入力を占有する）ので打った文字のキーはゲームへ届かない。
    pub fn tick<G: Game>(&mut self, game: &mut G, f: &UiFacts) -> UiOut {
        let out = self.tick_inner(game, f);
        self.sync_text_gate(game);
        self.sync_watch_feed();
        self.copy_tick(game, f.conn, f.now);
        self.update_tick(game, f.conn, f.now);
        out
    }

    /// 新しいバージョンの知らせを1 tick進める（`tick`から呼ぶ。`tick`が呼ばれないrollbackのマッチの間は`script_win::rb_scan_handler`が毎フレーム呼ぶ）。
    /// ゲームのスレッドで確保しない・待たない（確認は別のスレッドへ頼むだけ。見つかったかどうかはアトミック変数1つの読み取り）。
    ///
    /// 確認を頼むきっかけは、メニューが開いた・接続が成立した・接続している間にマッチ終了時のメニューが出た（立ち上がりだけ）。
    /// 確認の間隔と「見つかったら止まる」処理は`update_notice::Poll`。
    /// 見つかった後はメニューの一番下（`Menu::set_update`）に出す。
    /// 接続している間は、終わりのメニューが出ている間だけ左上（`ROW_UPD0` / `ROW_UPD1`）にも出す。
    /// 遅延の増分: 設定の無い実行・開発用のビルドは何もしない。確認が要る間は、接続している間だけ終わりのメニューの印を1ワード読む。
    pub fn update_tick<G: Game>(&mut self, game: &mut G, conn: Conn, now: u64) {
        let Some(hooks) = self.update_hooks else {
            return;
        };
        if !self.update_found.is_found() {
            let found = Found::from_raw((hooks.found)());
            if found.is_found() {
                self.update_found = found;
                self.update_poll.mark_found();
                self.menu.set_update(found);
            }
        }
        let linked = matches!(conn, Conn::Connected { .. });
        let menu_open = self.menu.is_active();
        let wants_end = self.update_poll.active() || self.update_found.is_found();
        let end_menu = linked && wants_end && game.end_view().shown;
        let seen = self.update_seen;
        for (rose, trigger) in [
            (menu_open && !seen.menu_open, Trigger::MenuOpened),
            (linked && !seen.linked, Trigger::Connected),
            (end_menu && !seen.end_menu, Trigger::EndMenu),
        ] {
            if rose && self.update_poll.due(trigger, now) {
                (hooks.ask)();
                break;
            }
        }
        self.update_seen = UpdateSeen {
            menu_open,
            linked,
            end_menu,
        };
        self.publish_update_rows(end_menu && !menu_open);
    }

    fn publish_update_rows(&mut self, show: bool) {
        let mut mask = self.shown_mask & !SHOW_UPDATE;
        let mut lines = [LineBuf::new(); UPDATE_NOTICE_LINES];
        if show && update_notice_lines(self.update_found, self.menu.lang(), &mut lines) > 0 {
            for ((line, row), bit) in lines
                .iter()
                .zip([ROW_UPD0, ROW_UPD1])
                .zip([SHOW_UPD0, SHOW_UPD1])
            {
                self.put(row, line.as_str(), RowStyle::Notice);
                mask |= bit;
            }
        }
        self.set_mask(mask);
    }

    /// 観戦コードのコピー（Ctrl+C）を1 tick進める。ゲームのスレッドで確保しない。
    /// 入力を占有するtickもマッチの中も毎tick呼ぶ（rollbackの間は`tick`が呼ばれないので`script_win::rb_scan_handler`からも呼ぶ）。
    ///
    /// 接続している間（`Conn::is_linked`: 見つかった・接続した）だけキーを読む。取り方はCtrl+数字と同じ（`Game::digit_keys`。押した瞬間だけ・Ctrlが押されている間だけ読む）。
    /// 押した瞬間に監視の処理へ依頼を1回送る（ホストなら観戦コードがクリップボードへ入り、ゲストなら「ホストの画面で」という知らせが返る）。
    /// 接続する前・台本の実行は本物のキーを読まない（台本の実行では段階の`copy=1`を押下とみなす）。
    /// 結果のイベント（`Event::is_copy_result`）は左上に`COPY_NOTICE_SECS`秒出す。
    /// ホストには接続した直後の左上（「つながりました」の行と同じ間）に案内を出す（相手が観戦を許可していないときは、ホストにもゲストにも案内の代わりにその旨を出す）。
    ///
    /// 遅延の増分: 接続している間は`digit_keys`を1回読む（同じtickにCtrl+数字の読み取りが同じ`NetGame`で済んでいれば足さない）。
    pub fn copy_tick<G: Game>(&mut self, game: &mut G, conn: Conn, now: u64) {
        let linked = conn.is_linked();
        let held = if linked && !game.scripted_run() {
            game.digit_keys()
        } else {
            None
        };
        let pressed = self.copy_edge.feed(held);
        // 台本の実行は本物のキーを読まない代わりに段階の`copy=1`を押下とみなす（実機の検証で人のクリップボードを使わず`clipboard = file`の置き場へ書かせる）。
        let scripted = linked && game.scripted_run() && game.copy_script();
        if digit_keys::copy_pressed(pressed) || scripted {
            let seq = self.link.note().map_or(0, |n| n.seq);
            self.link.send(Cmd::CopySpectateCode);
            self.copy_wait = Some((seq, now.saturating_add(COPY_WAIT_SECS)));
            self.stats.orders_sent = self.stats.orders_sent.saturating_add(1);
            self.stats.copy_asks = self.stats.copy_asks.saturating_add(1);
        }
        if let Some((seq0, until)) = self.copy_wait {
            if now >= until {
                self.copy_wait = None;
            } else if let Some(n) = self.link.note()
                && n.seq != seq0
                && n.event.is_copy_result()
            {
                self.copy_wait = None;
                self.copy_shown = Some((n.event, now.saturating_add(COPY_NOTICE_SECS)));
            }
        }
        if self.copy_shown.is_some_and(|(_, until)| now >= until) {
            self.copy_shown = None;
        }
        let lang = self.menu.lang();
        let line = match (self.copy_shown, conn) {
            (Some((ev, _)), _) if linked => copy_notice(ev, lang),
            (
                None,
                Conn::Connected {
                    watch_off: true, ..
                },
            ) if self.copy_hint_on && self.connected_line_shows() => {
                Some((watch_off_hint(lang), RowStyle::Hint))
            }
            (None, Conn::Connected { host: true, .. })
                if self.copy_hint_on && self.connected_line_shows() =>
            {
                Some((copy_hint(lang), RowStyle::Hint))
            }
            _ => None,
        };
        match line {
            Some((text, style)) => {
                self.put(ROW_COPY, text, style);
                self.set_mask(self.shown_mask | SHOW_COPY);
            }
            None if self.shown_mask & SHOW_COPY != 0 => {
                self.set_mask(self.shown_mask & !SHOW_COPY);
            }
            None => {}
        }
    }

    /// 一覧の接続をメニューが一覧の画面かどうか（`Menu::wants_watch_list`）に合わせる。
    /// 一覧の画面を出る経路（キャンセル・行を選ぶ・接続する・画面が替わる）はどれでも、ここで接続を閉じる。
    /// 入るtickに開け、出るtickに閉じる（開ける仕事はスレッドを作る。人が操作したtickの1回だけ）。
    fn sync_watch_feed(&mut self) {
        let want = self.menu.wants_watch_list();
        if want == self.watch_on {
            return;
        }
        self.watch_on = want;
        self.watch.set(ListStatus::Loading, &[]);
        if want {
            // 前に開いたときの一覧がバッファに残っていてもこれから書かれるものだけを読む。
            self.watch_seen = self.link.watch_list_version();
        }
        if let Some(feed) = self.watch_feed {
            feed(want);
        }
    }

    /// 一覧の画面の間は接続のスレッドが書いた新しい一覧を読んでメニューへ渡す。
    fn refresh_watch(&mut self) {
        if self.watch_on
            && self
                .link
                .read_watch_list(&mut self.watch_scratch, &mut self.watch_seen)
        {
            core::mem::swap(&mut self.watch, &mut self.watch_scratch);
        }
    }

    /// ウィンドウから文字を貯めるゲートをメニューが文字を欲しがっているかどうか（`Menu::wants_text`）に合わせる。
    /// 開けられなかったとき（ウィンドウへ文字を受ける仕組みを設定できない）はメニューへ知らせて、入力の画面に断りの行を出させる。
    /// 開けようとするのは入力1回につき1回だけ（`text_gate_tried`）。
    /// ウィンドウが見つからなかったときの探し直しは、ウィンドウを列挙する重い呼び出しなので毎tickはやらない。
    fn sync_text_gate<G: Game>(&mut self, game: &mut G) {
        let want = self.menu.wants_text();
        if !want {
            self.text_gate_tried = false;
        }
        if want == self.text_gate || (want && self.text_gate_tried) {
            return;
        }
        if want {
            self.text_gate_tried = true;
            let ok = game.text_gate(true);
            self.text_gate = ok;
            self.menu.set_text_available(ok);
        } else {
            game.text_gate(false);
            self.text_gate = false;
        }
    }

    fn tick_inner<G: Game>(&mut self, game: &mut G, f: &UiFacts) -> UiOut {
        self.refresh_note();
        self.refresh_watch();
        // 後始末した後の終わりの行は、人が入口のメニューを開いたか次のネットワークを開き始めたら役目を終える。
        if f.net != NetPhase::Closed || self.menu.is_active() {
            self.ended = None;
        }
        if let Some(out) = self.return_tick(game, f) {
            return out;
        }
        let found = matches!(f.conn, Conn::Found { .. });
        let found_edge = found && !self.was_found;
        self.was_found = found;
        let (cur, next) = game.scenes();
        // 場面2への切替のtickはMainMenuが取り除かれうるので読まない（`Game::menu_view`）。
        if cur != SCENE_MENU || next == SCENE_MATCH {
            self.scene = if cur == SCENE_MATCH || next == SCENE_MATCH {
                split_kb::Scene::Match
            } else {
                split_kb::Scene::Other
            };
            self.leave_menu_scene(f);
            return UiOut::default();
        }
        let Some(v) = game.menu_view() else {
            self.scene = split_kb::Scene::Other;
            self.leave_menu_scene(f);
            return UiOut::default();
        };
        self.scene = split_kb::Scene::Menu {
            screen: v.screen,
            select_kind: v.select_kind,
            accepting: higanr_core::menu_model::reads_presses(v.sub, v.trans),
        };
        // 画面6はメニューが入力を占有して`split_step`が呼ばれないので、画面の遷移はここで毎tick記録する。
        // 割り当てを解除したイベントは`split_step`を通らないtickでも取りこぼさないよう、ここで取り出して書く。
        self.split.observe(self.scene);
        if let Some(ev) = self.split.take_event() {
            crate::split_kb_win::log_event(ev);
        }
        self.menu_screen = Some(v.screen);
        let connected = matches!(f.conn, Conn::Connected { .. });
        // 自動の前進をかける間は次のどちらか: 成立した対戦する側・ホストの流れを受けている観戦者（`watch_advance`）。
        // 取消の確認の枠・難易度の画面を占有するかどうかの判断は`connected`（対戦する側）のまま。
        let advancing = connected || self.watch_advance;
        let parked = matches!(f.conn, Conn::Found { ready: true, .. });
        if advancing && !self.was_connected {
            self.auto = Some(0);
            self.stats.auto_starts = self.stats.auto_starts.saturating_add(1);
            // 観戦者: 観戦の「ルームマッチ」を決定したメニューは待ちの項目で開いたままなので、ホストの流れを受け始めたら閉じる。
            // 開いたままだと下の1. でメニューが入力を占有して3. の自動の前進が走らず、観戦者のゲームが画面6に留まる。
            if self.watch_advance && !connected && self.menu.is_active() {
                self.menu.reset();
            }
        }
        if !advancing {
            self.auto = None;
        }
        self.was_connected = advancing;
        // 自動の前進はMatchの画面の中だけ。画面を出た（難易度の画面へ進んだ・タイトルなどへ移った）ら終わり。
        if v.screen != SCREEN_MATCH_MODE {
            self.auto = None;
        }
        self.menu.set_opening(f.net == NetPhase::Opening);
        let difficulty = v.screen == u32::from(SCREEN_DIFFICULTY);
        let mine = v.screen == SCREEN_MATCH_MODE
            || (difficulty
                && (connected || self.menu.is_active() || self.menu.has_pending_cancel()));
        if !mine {
            // 触らない。メニューが開いていたなら（画面がゲームの都合で移った）閉じる。
            if self.menu.is_active() || self.menu.has_pending_cancel() {
                self.menu.reset();
            }
            self.publish_corner(f);
            return UiOut::default();
        }
        self.stats.held_ticks = self.stats.held_ticks.saturating_add(1);
        // 画面6は3本とも自分で読んで渡す（`Game::menu_words`）。ゲームは`[2]`だけを読むので2Pのパッドも入る[2]のワードで判定する。
        // 画面7は今までどおり自分のサイドの1ワードをネットの選択へ登録する（`stash_local`）。
        let on_match = v.screen == SCREEN_MATCH_MODE;
        let devs: [u16; 3] = if on_match {
            game.menu_words(f.seat)
        } else {
            let w = game.local_input(f.seat);
            [w, 0, w]
        };
        let word = devs[2];
        let cancel_edge = word & !self.prev_word & CANCEL_BITS != 0;
        self.prev_word = word;
        let mut out = UiOut::default();
        let ev = EntryView {
            screen: v.screen,
            sub: v.sub,
            trans: v.trans,
            cursor: v.cursor,
        };
        // 0. 接続した瞬間にMatchの画面で入口のメニューを開いていた側は選択済みにする。
        //    メニューを閉じていた側と別の画面にいた側は未選択のままにする（左上の行と音で知らせ、自分で選ばせる）。
        if found_edge && on_match && self.menu.is_open() && !parked {
            self.choose(&mut out);
        }
        // 1. 開いているメニュー（入口のメニュー・確認の枠・閉じ始め）。
        self.feed_vpn_nets();
        if self.menu.is_active() {
            let mut text = TextIn::NONE;
            if self.menu.wants_text() && self.text_gate {
                text.len = game.typed(&mut text.units).min(text.units.len());
            }
            let o = self.menu.tick_with(
                &TickIn {
                    word,
                    note: self.note,
                    conn: f.conn,
                    text,
                },
                &self.watch,
            );
            if let Some(c) = o.cmd {
                self.link.send(c);
                // 名前は監視の処理への指示ではない（バッファに書いただけ）ので数えない。iniへは別のスレッドが書く（人が名前を決定したtickに1回）。
                if let Cmd::Name(n) = c {
                    if let Some(save) = self.name_saver {
                        save(n);
                    }
                } else if let Cmd::RoomSet(n) = c {
                    // 設定で名を決定しただけでも使った名として覚える（指示ではないので数えない）。
                    if let Some(save) = self.room_saver {
                        save(n);
                    }
                } else if let Cmd::Lang(l) = c {
                    if let Some(save) = self.lang_saver {
                        save(l);
                    }
                } else if let Cmd::LanClient(a) | Cmd::LanWatch(a) = c {
                    // 決定したアドレスを使った値として覚える（指示のリングバッファは通らないので数えない）。
                    if let Some(save) = self.lan_saver {
                        save(a);
                    }
                } else if c != Cmd::LanHost {
                    self.stats.orders_sent = self.stats.orders_sent.saturating_add(1);
                }
                out.lan = match c {
                    Cmd::LanHost => Some(LanAsk::Host),
                    Cmd::LanClient(a) => Some(LanAsk::Client(a)),
                    Cmd::LanWatch(a) => Some(LanAsk::Watch(a)),
                    _ => None,
                };
                // 替えた設定はiniの該当の行へ書く（指示は指示として数える）。
                match c {
                    Cmd::Shape(pick, delay) => {
                        if let Some(save) = self.shape_saver {
                            save(pick, delay);
                        }
                    }
                    Cmd::Seat(wish) => {
                        if let Some(save) = self.seat_saver {
                            save(wish);
                        }
                    }
                    _ => {}
                }
                if matches!(c, Cmd::Create | Cmd::Paste | Cmd::Room(_) | Cmd::Random)
                    && f.net == NetPhase::Closed
                {
                    out.open_net = true;
                }
                // 観戦の「ルームマッチ」は対戦する側のネットワークを開かない。呼び手が観戦者のネットワークを（別のスレッドで）開く。
                if matches!(
                    c,
                    Cmd::Spectate(_) | Cmd::SpectateListed(_) | Cmd::SpectateCode
                ) {
                    out.spectate_open = true;
                }
            }
            if o.answer == Some(Answer::Leave) {
                out.leave = true;
                self.left_by_menu = true;
                self.auto = None;
                self.stats.leaves = self.stats.leaves.saturating_add(1);
            }
            if o.hold_game {
                return self.hold(game, f, out);
            }
        }
        // 2. 確認の枠で「はい」と答えた後の取消を、閉じ終えた次のtickに1回だけゲームへ通す。
        if self.menu.take_pending_cancel() {
            Self::feed3(game, KEY_CANCEL, 0);
            self.publish_state(f);
            out.action = Some(Action::Menu);
            return out;
        }
        // 3. 自動の前進（成立した後にMatchの画面を「人間 対 人間」まで進める）。
        //    使うのは`AUTO_MAX_TICKS` tickまで（進まないまま押し続けないための上限。超えたら前進をやめる）。
        if on_match
            && advancing
            && let Some(n) = self.auto
        {
            if n >= AUTO_MAX_TICKS {
                self.auto = None;
            } else {
                self.auto = Some(n + 1);
                let im2: InputState = game.input_state(2);
                match ui_entry::advance(&ev, &im2) {
                    Advance::Press(w) => {
                        self.stats.auto_presses = self.stats.auto_presses.saturating_add(1);
                        Self::feed3(game, w, 0);
                        self.publish_state(f);
                        out.action = Some(Action::Menu);
                        return out;
                    }
                    Advance::Idle => return self.hold(game, f, out),
                }
            }
        }
        // 4. 次のキャンセルはゲームへ渡さず飲み込んで確認の枠を出す。
        //    ・成立した後に難易度の画面より前へ戻るキャンセル（Matchの画面から・難易度の画面から）
        //    ・選択済みで相手を待っている間のMatchの画面のキャンセル
        //    選択を取り消す経路は「抜ける」だけ（`match_gate`の「単調」）。
        if (connected || (parked && on_match)) && cancel_edge {
            self.menu.open_confirm(word);
            self.stats.confirms = self.stats.confirms.saturating_add(1);
            return self.hold(game, f, out);
        }
        // 4'. 選択済みで相手を待っている間のMatchの画面は何も押さない（カーソルを動かして別の項目を決定しない）。
        //     成立したら自動の前進がMatchの画面から進める。
        if parked && on_match {
            return self.hold(game, f, out);
        }
        // 5. Matchの画面（入口）: 「妖怪 対 妖怪」の決定の判定に使うのはゲームが読む`[2]`のワード（`devs[2]`）。
        //    ゲームへ漏れると判定したtickは3本とも0を渡す。それ以外は`poll_all`と同じ3本（1P・2Pの機器のワードと合成）をそのまま渡す。
        if on_match {
            let im2: InputState = game.input_state(2);
            if ui_entry::leaks(&ev, &im2, word) {
                if found {
                    // 接続した後・成立の前: 「妖怪 対 妖怪」を選択済みにする（メニューは開かない。接続した後のメニューには項目が無い）。
                    self.choose(&mut out);
                    self.stats.intercepts = self.stats.intercepts.saturating_add(1);
                    return self.hold(game, f, out);
                }
                self.menu.open(word);
                self.ended = None;
                self.stats.opened = self.stats.opened.saturating_add(1);
                self.stats.intercepts = self.stats.intercepts.saturating_add(1);
                return self.hold(game, f, out);
            }
            for (i, w) in devs.into_iter().enumerate() {
                game.feed(i as u32, w);
            }
            self.publish_state(f);
            out.action = Some(Action::Menu);
            return out;
        }
        game.stash_local(word);
        self.publish_corner(f);
        out
    }

    pub fn hud(&mut self, on: bool, h: HudFacts) {
        if !on {
            if self.shown_mask & SHOW_MATCH != 0 {
                let m = self.shown_mask & !SHOW_MATCH;
                self.set_mask(m);
            }
            self.hud_names = None;
            return;
        }
        if let Some(r) = h.rewound {
            self.rewind.push(r);
        }
        let mut line = LineBuf::new();
        hud_line(
            &mut line,
            self.menu.lang(),
            &HudView {
                latency: h.latency,
                rtt_ms: h.rtt_ms,
                recommend: h.recommend,
                rewind: h.rewound.map(|_| self.rewind.max()),
                auto: h.auto,
                format: h.format,
                delay_only: h.delay_only,
            },
        );
        self.put(ROW_HUD, line.as_str(), RowStyle::Normal);
        self.set_hud_right(self.status_side.on_right(h.seat.unwrap_or(Seat::P1)));
        self.show_match_rows(h.names);
    }

    /// 通信の状態の行の置き場所を盤へ書く（変わったときだけ）。
    fn set_hud_right(&mut self, right: bool) {
        if right != self.hud_right {
            self.board.set_hud_right(right);
            self.hud_right = right;
        }
    }

    /// 観戦者のマッチ中の表示を更新する（フレームごと。`on`が偽なら消す）。
    /// 対戦する側の`hud`と同じ置き場・同じ見た目で、名前の帯と左下の通信の状態の行（`watch_hud_line`）を出す。値が変わったときだけ盤へ書く。
    pub fn watch_hud(&mut self, on: bool, w: WatchHudFacts) {
        if !on {
            self.hud(false, HudFacts::default());
            return;
        }
        let mut line = LineBuf::new();
        watch_hud_line(
            &mut line,
            self.menu.lang(),
            &WatchHudView {
                stats: w.stats,
                delay_frames: w.delay_frames,
                fast_forward: w.fast_forward,
            },
        );
        self.put(ROW_HUD, line.as_str(), RowStyle::Small);
        self.set_hud_right(false);
        self.show_match_rows(w.names);
    }

    /// 相手を待つ知らせの1行を出す・消す（tickごと）。変わったtickだけ盤へ書く（ふだんの増分は比較1回）。
    pub fn peer_wait(&mut self, on: bool) {
        self.wait_notice(on, T_PEER_WAIT, RowStyle::Warn);
    }

    /// 観戦者が次のキャラ選択まで映らない間の知らせ（`watch_notice`の1つ）。
    pub fn watch_select_wait(&mut self, on: bool) {
        self.watch_notice(on.then_some(WatchNotice::NextSelect));
    }

    /// 観戦者の知らせの1行を出す・替える・消す（tickごと。`peer_wait`と同じ置き場。観戦者のセッションでは`peer_wait`は呼ばれない）。
    /// 変わったtickだけ盤へ書く。
    pub fn watch_notice(&mut self, n: Option<WatchNotice>) {
        if n == self.watch_notice_shown && (n.is_some() == (self.shown_mask & SHOW_PEER_WAIT != 0))
        {
            return;
        }
        match n {
            Some(n) => {
                let text = match n {
                    WatchNotice::NextSelect => T_SPECTATE_NEXT_SELECT,
                    WatchNotice::Paused => T_SPECTATE_PAUSED,
                    WatchNotice::EscHold => T_SPECTATE_ESC_HOLD,
                };
                self.put(ROW_PEER_WAIT, text.get(self.menu.lang()), RowStyle::Normal);
                self.set_mask(self.shown_mask | SHOW_PEER_WAIT);
            }
            None => self.set_mask(self.shown_mask & !SHOW_PEER_WAIT),
        }
        self.watch_notice_shown = n;
    }

    fn wait_notice(&mut self, on: bool, text: Tx, style: RowStyle) {
        if on == (self.shown_mask & SHOW_PEER_WAIT != 0) {
            return;
        }
        if on {
            self.put(ROW_PEER_WAIT, text.get(self.menu.lang()), style);
        }
        self.set_mask(self.shown_mask ^ SHOW_PEER_WAIT);
    }

    fn show_match_rows(&mut self, names: Option<HudNames>) {
        let mut m = self.shown_mask | SHOW_HUD;
        match names {
            Some(n) => {
                if self.hud_names != Some(n) {
                    let mut l = LineBuf::new();
                    name_band_line(&mut l, self.menu.lang(), Seat::P1, n.p1.as_bytes());
                    self.put(ROW_NAME_L, l.as_str(), RowStyle::Normal);
                    name_band_line(&mut l, self.menu.lang(), Seat::P2, n.p2.as_bytes());
                    self.put(ROW_NAME_R, l.as_str(), RowStyle::Normal);
                    self.hud_names = Some(n);
                }
                m |= SHOW_NAMES;
            }
            None => {
                self.hud_names = None;
                m &= !SHOW_NAMES;
            }
        }
        self.set_mask(m);
    }
}

// ---- 対戦の後に接続が終わったら「妖怪 対 妖怪」へ戻す（判断は`higanr_core::return_flow`）----
//
// 始まるのは`net_input`が`NetPlay::take_lost`で理由を受けたtick。以後は場面を問わず`tick_inner`の先頭で1 tickに1回進める。
// ネットワークのセッションを後始末した後（`ui_only_step`）も続ける。ゲームのメモリには書かず、入力のワードだけを作る。
// 接続している試合のtickに足す処理は、`ret`があるかどうかの比較1回だけ。

/// 戻り道が読む人のワードとInputManager（読むのは要るtickだけ）。
struct FlowReads<'a, G: Game> {
    game: &'a mut G,
    seat: Seat,
}

impl<G: Game> Reads for FlowReads<'_, G> {
    fn raw(&mut self) -> u16 {
        // 一時停止・終わりのメニューが読む`[2]`のワード（素のpollの[2] = キーボードと両方のパッド）。
        self.game.menu_words(self.seat)[2]
    }
    fn im(&mut self, index: usize) -> InputState {
        self.game.input_state(index)
    }
}

impl NetUi {
    /// 対戦の後に接続が終わったら「妖怪 対 妖怪」へ戻す。`chime`は戻り始めに1回呼ぶ音を鳴らす処理。
    /// ゲームのスレッドから呼ぶので、スレッドを作るだけにする。
    pub fn with_return_flow(mut self, chime: Option<fn()>) -> NetUi {
        self.ret_on = true;
        self.lost_chime = chime;
        self
    }

    /// 戻り道の集計を本ログへ出す処理を設定する（始まったとき・着いたとき・途中でやめたときに呼ぶ。ゲームのスレッドで確保しない処理に限る）。
    pub fn with_return_log(mut self, log: Option<fn(&UiStats)>) -> NetUi {
        self.ret_log = log;
        self
    }

    fn log_return(&self) {
        if let Some(log) = self.ret_log {
            log(&self.stats);
        }
    }

    /// 成立した後にネットワークが終わったときに呼ぶ（`NetPlay::take_lost`。1接続に1回）。
    /// 確認の枠で自分から抜けたときは戻さない（キャンセルでゲームが自分で戻る）。
    pub fn on_lost(&mut self, why: Why) {
        let by_menu = core::mem::take(&mut self.left_by_menu);
        if !self.ret_on || (by_menu && why != Why::PeerLeft) {
            return;
        }
        self.ret = Some(Flow::new());
        self.lost = Some(why);
        // 開いていた確認の枠と、その後に通すキャンセルを捨てる（戻り道が入力を占有する）。
        self.menu.reset();
        self.auto = None;
        self.stats.returns = self.stats.returns.saturating_add(1);
        self.log_return();
        if why != Why::SelfCut
            && let Some(chime) = self.lost_chime
        {
            chime();
        }
    }

    #[cfg(test)]
    pub fn returning(&self) -> bool {
        self.ret.is_some()
    }

    /// 戻り道の1 tick。入力を占有したら`Some`を返す（feedは済ませてある）。
    /// 占有しないtick（保存の画面・やめたtick）は`None`を返し、呼び手はいつもの処理へ進む。
    fn return_tick<G: Game>(&mut self, game: &mut G, f: &UiFacts) -> Option<UiOut> {
        if self.ret.is_none() && (self.menu.is_active() || f.conn.is_linked()) {
            self.lost = None;
        }
        let flow = self.ret.as_mut()?;
        let (cur, next) = game.scenes();
        let facts = Facts {
            in_match: cur == SCENE_MATCH && next == SCENE_MATCH,
            in_menu: cur == SCENE_MENU && next == SCENE_MENU,
            save_quit: cur == SCENE_MATCH && next == REQ_SAVE_AND_QUIT,
            play: if cur == SCENE_MATCH {
                game.play_view()
            } else {
                None
            },
            // 場面2への切替のtickはMainMenuが取り除かれうるので読まない（`Game::menu_view`）。
            menu: if cur == SCENE_MENU && next != SCENE_MATCH {
                game.menu_view().map(|v| Screen {
                    id: v.screen,
                    sub: v.sub,
                    trans: v.trans,
                    cursor: v.cursor as i32,
                    decided: v.decided,
                })
            } else {
                None
            },
        };
        let s = flow.step(
            &facts,
            &mut FlowReads {
                game: &mut *game,
                seat: f.seat,
            },
        );
        match s.finish {
            Some(Finish::Arrived) => {
                self.ret = None;
                self.stats.return_arrived = self.stats.return_arrived.saturating_add(1);
                self.log_return();
            }
            Some(Finish::GaveUp(why)) => {
                self.ret = None;
                self.stats.return_gave_up = self.stats.return_gave_up.saturating_add(1);
                self.stats.return_why = why.encode();
                self.log_return();
            }
            None => {}
        }
        let mut out = UiOut {
            save_quit: s.save_quit,
            ..UiOut::default()
        };
        match s.out {
            return_flow::Out::Pass => return None,
            return_flow::Out::Freeze => out.action = Some(Action::RenderOnly),
            return_flow::Out::Feed(w) => {
                for (i, x) in w.into_iter().enumerate() {
                    game.feed(i as u32, x);
                }
                out.action = Some(Action::Menu);
            }
        }
        self.publish_corner(f);
        Some(out)
    }
}

/// ネットワークの経路（ネットワークのセッションが動いている実行）の1 tickの入力の部分。順序がこの関数の要。
///
/// 1. ネットワークの保守処理（`NetPlay::tend`）。**メニューが入力を占有するtickも毎tick先に呼ぶ**（ここで保守処理を止めると貼っても接続できない）。
/// 2. メニュー（`ui`）。占有したら（`UiOut::action`が`Some`）そのactionを返し、場面ごとの判断（`dispatch`）は呼ばない（feedの二重化を防ぐ）。
/// 占有しなければ`dispatch`を呼ぶ。
///
/// ネットワークのセッションが動いているので`UiFacts::net`は`Open`（`open_net`は出ない）。`ui`が`None`（メニューを設定していない実行）なら保守処理 → 判断の順に進む。
pub fn net_input<G: Game, N: Net>(
    ui: Option<&mut NetUi>,
    game: &mut G,
    np: &mut NetPlay,
    net: &mut N,
    now: u64,
) -> (Action, UiOut) {
    np.tend(net);
    let out = match ui {
        Some(ui) => {
            if let Some(why) = np.take_lost() {
                ui.on_lost(why);
            }
            let (conn, seat) = np.ui_conn((net.now_us() / 1_000) as u32);
            // 作り直しが1回でもあったとき、または相手のコードを読んだ後だけ、自分のネットワーク種別のフラグを読む（アトミック変数が数個）。確保しない。
            let r = np.restarts();
            let peer = np.peer_nat();
            let stall = if r.count > 0 || peer.is_some() {
                let fail = higanr_core::nat::choose(net.own_nat(), peer);
                if r.count > 0 {
                    Stall::from_restarts(r, fail)
                } else {
                    Stall::predicted(fail)
                }
            } else {
                None
            };
            let f = UiFacts {
                conn,
                seat,
                now,
                net: NetPhase::Open,
                stall,
            };
            let out = ui.tick(game, &f);
            if out.ready {
                // 自分が選択済みになった: READYの1通目をこのtickに送る（成立は相手のREADYを受けたtickの保守処理で判定する）。
                np.set_ready(net);
            }
            if out.leave {
                np.close(ByeReason::Cancelled);
            }
            out
        }
        None => UiOut::default(),
    };
    let action = match out.action {
        Some(a) => a,
        None => np.dispatch(game, net),
    };
    (action, out)
}

/// 観戦者のセッションが動いている実行（ゲーム内のメニューの観戦の「ルームマッチ」）の1 tickの入力の部分。`net_input`の観戦者版で、順序が要。
///
/// 1. ネットワークの保守処理（`WatchPlay::tend`）。**メニューが入力を占有するtickも毎tick先に呼ぶ**（最初のtickが受け取りを受信スレッドへ渡し、以後は接続・終わりを監視する）。
/// 2. 自分のEscの読み取り（`WatchPlay::esc_edge`）。**メニューより先に呼ぶ**。`peek_local_input`が台本をこのtickの分だけ進め、続くメニューの`menu_words`は進めた結果を使う（台本は1 tickに1回。二重に進めない）。
/// 3. メニュー（`NetUi::tick`）。画面6は占有する。ホストの流れを受けている間（`Phase::Selecting`）は、接続した対戦する側と同じ自動の前進で画面7まで進める。Escを3秒長押しすると占有したtickでも観戦をやめる（`esc_edge`が数える）。
/// 4. 占有しなければ`WatchPlay::dispatch`を呼ぶ（記録の列がゲームを動かす・自分の入力を通す）。
///
/// 左上の行と項目は監視の処理（`spectate_join`）が書く状態から出す。接続の状態は`Conn::Waiting`のまま（`Conn::Connected`は対戦する側の行になる）。
/// 観戦者のセッションは対戦のセッションではないので`NetPhase::Open`になる（`open_net`は出ない）。
pub fn watch_input<G: Game, N: Net>(
    ui: &mut NetUi,
    game: &mut G,
    wp: &mut WatchPlay,
    net: &mut N,
    now: u64,
) -> (Action, UiOut) {
    wp.tend(net);
    let esc = wp.esc_edge(game);
    ui.set_watch_advance(wp.phase() == Phase::Selecting);
    let f = UiFacts {
        conn: Conn::Waiting,
        seat: Seat::P1,
        now,
        net: NetPhase::Open,
        stall: None,
    };
    let out = ui.tick(game, &f);
    // マッチ中の表示（名前の帯と通信の状態の行）は判断（`dispatch`）の前に読む。読むだけで状態は動かさない。
    // 1 tick古い値になるが、帯は変わったときだけ書くので見た目は変わらない。マッチの外では消す。
    match wp.hud_facts() {
        Some(h) => ui.watch_hud(true, h),
        None => ui.watch_hud(false, WatchHudFacts::default()),
    }
    // メニューが占有するtickはEscの長押し（`esc_edge`が見る）のほかは観戦者の判断を通さない。
    let action = match out.action {
        Some(a) => a,
        None => wp.dispatch(game, net, esc),
    };
    // 知らせは判断の後に読む（Escを押したtickに案内・一時停止の行が出る）。
    ui.watch_notice(wp.notice());
    (action, out)
}

#[cfg(test)]
mod return_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::lockstep::Role;
    use higanr_core::match_gate::NoMatch;
    use higanr_core::menu_model::DECIDE_BITS;
    use higanr_core::net_menu::{
        CodeState, Event, T_AGAIN, T_SPECTATE_CODE_COPIED, T_SPECTATE_CODE_COPY_FAILED,
        T_SPECTATE_CODE_HINT, T_SPECTATE_CODE_HOST_ONLY, T_SPECTATE_CODE_PEER_OFF,
        T_SPECTATE_PEER_OFF,
    };
    use higanr_core::overlay::{blit_quads, ui_glyphs};
    use higanr_core::selection::Agreed;
    use higanr_core::wire::{Mode, Select};

    use crate::net_select::MenuView;
    use crate::netplay::BadValue;

    struct Fake {
        scenes: (u32, u32),
        view: Option<MenuView>,
        words: std::collections::VecDeque<u16>,
        im2: InputState,
        feeds: Vec<[u16; 3]>,
        cur_feed: [Option<u16>; 3],
        local_reads: u32,
        stashed: Option<u16>,
        stash_calls: u32,
        /// 2Pの機器（素のpollの[1]）のワード。`menu_words`の2本目で、[2]（合成）のワードは1Pのワードにこれを足したもの。
        dev1: u16,
        /// ウィンドウから貯まった文字（`Game::typed`が古い順に渡す）。本物はゲートが閉じている間は貯めない。
        /// 偽物は貯めたまま持っておき、ゲートが開いている間にだけ渡す。
        typed: std::collections::VecDeque<u16>,
        gate_calls: Vec<bool>,
        gate_ok: bool,
        gate_open: bool,
        /// ゲートが閉じている間に`typed`が呼ばれた回数（閉じている間は取りに行かない約束）。
        typed_while_closed: u32,
        /// 観戦者のEscの読み取り（`peek_local_input`）が返すワード。`local_input`のワードとは別に読むので`words`を消費しない。
        esc_word: u16,
        /// 本物のキーの読み取り（`Game::digit_keys`）の代わり。`None`は読んではいけない間。
        keys: Option<u16>,
        /// `digit_keys`を呼ばれた回数。
        keys_reads: u32,
        /// 台本の実行（`Game::scripted_run`）。
        scripted: bool,
        /// 台本の`copy=1`（`Game::copy_script`）。次の1回だけ`true`を返す。
        copy_script_next: bool,
        /// マッチ終了時のメニューが出ている（`Game::end_view`）。
        end_menu: bool,
        /// `end_view`を読まれた回数。
        end_reads: u32,
    }

    impl Fake {
        fn on_screen(screen: u32, cursor: u32) -> Fake {
            Fake {
                scenes: (SCENE_MENU, 0),
                view: Some(MenuView {
                    screen,
                    sub: 1,
                    trans: 0,
                    cursor,
                    ..MenuView::default()
                }),
                words: Default::default(),
                im2: InputState::new(),
                feeds: Vec::new(),
                cur_feed: [None; 3],
                local_reads: 0,
                stashed: None,
                stash_calls: 0,
                dev1: 0,
                typed: Default::default(),
                gate_calls: Vec::new(),
                gate_ok: true,
                gate_open: false,
                typed_while_closed: 0,
                esc_word: 0,
                keys: None,
                keys_reads: 0,
                scripted: false,
                copy_script_next: false,
                end_menu: false,
                end_reads: 0,
            }
        }

        fn on_match_screen(cursor: u32) -> Fake {
            Fake::on_screen(6, cursor)
        }

        /// 1 tickを回す（ワード`w`を押す）。ゲームの`feed`が届いたら、そのワードでInputManager[2]の模型も進める。
        /// これで次のtickの入口の判定が前のfeedの続きになる。
        fn step(&mut self, ui: &mut NetUi, f: &UiFacts, w: u16) -> UiOut {
            self.words.push_back(w);
            self.cur_feed = [None; 3];
            let a = ui.tick(self, f);
            self.words.clear();
            if let [Some(a0), Some(a1), Some(a2)] = self.cur_feed {
                self.feeds.push([a0, a1, a2]);
                self.im2.feed(a2);
            }
            a
        }

        fn last_feed(&self) -> Option<[u16; 3]> {
            self.feeds.last().copied()
        }

        fn step_net<N: Net>(
            &mut self,
            ui: &mut NetUi,
            np: &mut NetPlay,
            net: &mut N,
            w: u16,
        ) -> (Action, UiOut) {
            self.words.push_back(w);
            self.cur_feed = [None; 3];
            let r = net_input(Some(ui), self, np, net, 1_000_000);
            self.words.clear();
            if let [Some(a0), Some(a1), Some(a2)] = self.cur_feed {
                self.feeds.push([a0, a1, a2]);
                self.im2.feed(a2);
            }
            r
        }
    }

    impl Game for Fake {
        fn scenes(&mut self) -> (u32, u32) {
            self.scenes
        }
        fn local_input(&mut self, _seat: Seat) -> u16 {
            self.local_reads += 1;
            self.words.pop_front().unwrap_or(0)
        }
        fn menu_words(&mut self, seat: Seat) -> [u16; 3] {
            let w = self.local_input(seat);
            [w, self.dev1, w | self.dev1]
        }
        fn peek_local_input(&mut self) -> u16 {
            self.esc_word
        }
        fn digit_keys(&mut self) -> Option<u16> {
            self.keys_reads += 1;
            self.keys
        }
        fn scripted_run(&self) -> bool {
            self.scripted
        }
        fn copy_script(&mut self) -> bool {
            std::mem::take(&mut self.copy_script_next)
        }
        fn end_view(&mut self) -> higanr_core::end_pick::View {
            self.end_reads += 1;
            higanr_core::end_pick::View {
                shown: self.end_menu,
                ..Default::default()
            }
        }
        fn feed(&mut self, index: u32, word: u16) {
            if let Some(s) = self.cur_feed.get_mut(index as usize) {
                assert!(s.is_none(), "同じ tick に index {index} を 2 回 feed した");
                *s = Some(word);
            }
        }
        fn own_select(&mut self, _s: Seat, _r: Role) -> Select {
            unreachable!("UI は SELECT を作らない")
        }
        fn apply_agreed(&mut self, _p: Seat, _a: &Agreed) -> Result<(), BadValue> {
            unreachable!()
        }
        fn write_charge(&mut self, _p1: u8, _p2: u8) -> bool {
            unreachable!()
        }
        fn check_values(&mut self) -> (u16, u32, u8, u8) {
            unreachable!()
        }
        fn menu_view(&mut self) -> Option<MenuView> {
            self.view
        }
        fn input_state(&mut self, index: usize) -> InputState {
            assert_eq!(index, 2, "判定は InputManager[2]");
            self.im2
        }
        fn stash_local(&mut self, word: u16) {
            self.stash_calls += 1;
            self.stashed = Some(word);
        }
        fn text_gate(&mut self, open: bool) -> bool {
            self.gate_calls.push(open);
            if open {
                self.gate_open = self.gate_ok;
                self.gate_ok
            } else {
                self.gate_open = false;
                true
            }
        }
        fn typed(&mut self, out: &mut [u16]) -> usize {
            if !self.gate_open {
                self.typed_while_closed += 1;
                return 0;
            }
            let n = out.len().min(self.typed.len());
            for d in out.iter_mut().take(n) {
                *d = self.typed.pop_front().unwrap_or(0);
            }
            n
        }
    }

    fn board() -> &'static Board {
        Box::leak(Box::new(Board::new()))
    }

    fn ini() -> Shape {
        Shape {
            mode: Mode::Rollback,
            delay: 1,
        }
    }

    fn facts() -> UiFacts {
        UiFacts {
            conn: Conn::Waiting,
            seat: Seat::P1,
            now: 1_000_000,
            net: NetPhase::Closed,
            stall: None,
        }
    }

    fn connected_facts() -> UiFacts {
        UiFacts {
            conn: Conn::Connected {
                seat: Seat::P1,
                host: false,
                shape: Some((Mode::Rollback, 1)),
                watch_off: false,
            },
            net: NetPhase::Open,
            ..facts()
        }
    }

    fn new_ui() -> (NetUi, Arc<UiLink>, &'static Board) {
        let link = Arc::new(UiLink::new());
        let b = board();
        (NetUi::new(ini(), Arc::clone(&link), b), link, b)
    }

    const Z: u16 = 0x0001;
    const X: u16 = 0x0002;
    const ESC: u16 = 0x0008;
    const UP: u16 = 0x0010;
    const DOWN: u16 = 0x0020;

    fn row_text(b: &Board, i: usize) -> String {
        let mut buf = [0u8; MAX_TEXT_UTF8];
        b.read_row(i, &mut buf)
            .map(|(t, _)| t.to_string())
            .unwrap_or_default()
    }

    fn menu_rows(b: &Board) -> Vec<String> {
        let n = b.menu_len.load(Ordering::Acquire) as usize;
        (0..n).map(|i| row_text(b, i)).collect()
    }

    #[test]
    fn orders_and_shape_cross_the_link_in_order() {
        let l = UiLink::new();
        assert_eq!((l.take_order(), l.shape()), (None, None));
        l.send(Cmd::Create);
        l.send(Cmd::Paste);
        l.send(Cmd::Cancel);
        assert_eq!(l.room_name(), None, "名を決める前に名が読める");
        l.send(Cmd::Room(RoomBuf::from_name("Synth-Room_1").unwrap()));
        assert_eq!(l.take_order(), Some(Order::Create));
        assert_eq!(l.take_order(), Some(Order::Paste));
        assert_eq!(l.take_order(), Some(Order::Cancel));
        assert_eq!(l.take_order(), Some(Order::Room));
        assert_eq!(l.take_order(), None);
        assert_eq!(l.room_name(), RoomBuf::from_name("Synth-Room_1"));
        assert_eq!(l.spectate_name(), None, "名を決める前に観戦の名が読める");
        l.send(Cmd::Spectate(RoomBuf::from_name("Watch-1").unwrap()));
        l.send(Cmd::SpectateCancel);
        assert_eq!(l.take_order(), Some(Order::Spectate));
        assert_eq!(l.take_order(), Some(Order::SpectateCancel));
        assert_eq!(l.take_order(), None);
        assert_eq!(l.spectate_name(), RoomBuf::from_name("Watch-1"));
        assert_eq!(l.room_name(), RoomBuf::from_name("Synth-Room_1"));
        // 観戦コード入力は観戦の決定と同じ指示。開く処理が印を見て観戦コードの経路を選ぶ。ルーム名で観戦を決定し直すと印は消える。
        assert!(!l.spectate_by_code());
        l.send(Cmd::SpectateCode);
        assert_eq!(l.take_order(), Some(Order::Spectate));
        assert!(l.spectate_by_code());
        l.send(Cmd::Spectate(RoomBuf::from_name("Watch-2").unwrap()));
        assert!(
            !l.spectate_by_code(),
            "ルーム名の観戦が観戦コードの経路のまま"
        );
        l.send(Cmd::SpectateCode);
        let row = watch_row(7, "a", "b", 0);
        l.send(Cmd::SpectateListed(row));
        assert!(!l.spectate_by_code(), "一覧の観戦が観戦コードの経路のまま");
        l.send(Cmd::CopySpectateCode);
        assert_eq!(l.take_order(), Some(Order::Spectate));
        assert_eq!(l.take_order(), Some(Order::Spectate));
        assert_eq!(l.take_order(), Some(Order::Spectate));
        assert_eq!(l.take_order(), Some(Order::SpectateCopy));
        assert_eq!(l.take_order(), None);
        for _ in 0..(ORDER_RING + 5) {
            l.send(Cmd::Create);
        }
        let mut n = 0;
        while l.take_order().is_some() {
            n += 1;
        }
        assert_eq!(n, ORDER_RING, "溢れた分は捨てる");
        l.send(Cmd::Paste);
        assert_eq!(l.take_order(), Some(Order::Paste), "取った後はまた入る");
        assert_eq!(l.start_delay(), None, "触っていない");
        for p in (0..=MAX_PICK_DELAY)
            .map(Pick::Rollback)
            .chain(std::iter::once(Pick::Delay))
        {
            for d in std::iter::once(DelayChoice::Auto)
                .chain((0..=MAX_PICK_DELAY).map(DelayChoice::Fixed))
            {
                l.send(Cmd::Shape(p, d));
                assert_eq!(l.shape(), Some(p), "{p:?} {d:?}");
                assert_eq!(l.start_delay(), Some(d), "{p:?} {d:?}");
            }
        }
    }

    #[test]
    fn the_note_box_round_trips_and_refuses_half_written() {
        let nb = NoteBox::new();
        assert_eq!(nb.read(), None, "まだ書かれていない");
        let n = Note {
            seq: 7,
            event: Event::Made,
            state: CodeState::Offered,
            code_len: 58,
            since: 99,
            ..Note::default()
        };
        nb.publish(&n);
        assert_eq!(nb.read(), Some(n));
        nb.seq.fetch_add(1, Ordering::SeqCst);
        assert_eq!(nb.read(), None);
    }

    #[test]
    fn the_yokai_decide_is_swallowed_and_other_screens_are_untouched() {
        let (mut ui, _link, _b) = new_ui();
        let mut g = Fake::on_match_screen(4);
        g.scenes = (2, 0);
        assert_eq!(g.step(&mut ui, &facts(), Z), UiOut::default());
        assert_eq!((g.local_reads, g.feeds.len()), (0, 0));
        g.scenes = (SCENE_MENU, SCENE_MATCH);
        g.view = None;
        assert_eq!(g.step(&mut ui, &facts(), Z), UiOut::default());
        assert_eq!(g.local_reads, 0);
        for screen in [1, 7, 8, 16] {
            let mut g = Fake::on_screen(screen, 4);
            assert_eq!(
                g.step(&mut ui, &facts(), Z),
                UiOut::default(),
                "screen {screen}"
            );
            assert_eq!(
                (g.local_reads, g.feeds.len(), g.stash_calls),
                (0, 0, 0),
                "screen {screen}"
            );
        }
        let mut g = Fake::on_match_screen(3);
        assert_eq!(g.step(&mut ui, &facts(), Z).action, Some(Action::Menu));
        assert_eq!(g.feeds, vec![[Z, 0, Z]]);
        assert!(!ui.menu_active());
        let mut g = Fake::on_match_screen(4);
        assert_eq!(g.step(&mut ui, &facts(), Z).action, Some(Action::Menu));
        assert_eq!(g.feeds, vec![[0, 0, 0]], "決定は漏れない");
        assert!(ui.menu_active());
        assert_eq!(ui.stats().intercepts, 1);
        assert_eq!(g.step(&mut ui, &facts(), Z).action, Some(Action::Menu));
        assert_eq!(g.feeds, vec![[0, 0, 0], [0, 0, 0]]);
    }

    #[test]
    fn the_entry_uses_the_move_in_the_same_tick_and_the_input_manager_state() {
        let (mut ui, _link, _b) = new_ui();
        let mut g = Fake::on_match_screen(3);
        assert_eq!(
            g.step(&mut ui, &facts(), DOWN | Z).action,
            Some(Action::Menu)
        );
        assert_eq!(
            g.feeds,
            vec![[0, 0, 0]],
            "3 で下 + 決定は 4 への決定なので奪う"
        );
        assert!(ui.menu_active());
        // Zを押したままのカーソル4の画面: 前のtickのfeedがZだった（押した瞬間ではない）なら奪わない。
        let (mut ui2, _l2, _b2) = new_ui();
        let mut g = Fake::on_match_screen(4);
        g.im2.feed(Z);
        g.step(&mut ui2, &facts(), Z);
        assert_eq!(g.feeds, vec![[Z, 0, Z]], "押しっぱなしは決定でない");
        assert!(!ui2.menu_active());
        let (mut ui3, _l3, _b3) = new_ui();
        let mut g = Fake::on_match_screen(4);
        ui3.auto = None;
        ui3.was_connected = true;
        g.step(&mut ui3, &connected_facts(), Z);
        assert_eq!(
            g.feeds,
            vec![[0, 0, 0]],
            "つながっていてもカーソル 4 の決定は漏らさない"
        );
        assert!(ui3.menu_active());
    }

    #[test]
    fn menu_commands_reach_the_link_and_closing_feeds_zero_until_released() {
        let (mut ui, link, _b) = new_ui();
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        assert!(g.step(&mut ui, &facts(), Z).open_net);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(link.take_order(), Some(Order::Create));
        let opening = UiFacts {
            net: NetPhase::Opening,
            ..facts()
        };
        assert!(!g.step(&mut ui, &opening, Z).open_net);
        g.step(&mut ui, &opening, 0);
        assert_eq!(link.take_order(), Some(Order::Create));
        let before = g.feeds.len();
        for _ in 0..5 {
            assert_eq!(g.step(&mut ui, &facts(), X).action, Some(Action::Menu));
        }
        assert!(g.feeds[before..].iter().all(|f| *f == [0, 0, 0]));
        assert!(ui.menu_active(), "X を押している間は閉じ終わらない");
        assert_eq!(g.step(&mut ui, &facts(), 0).action, Some(Action::Menu));
        assert!(!ui.menu_active());
        assert_eq!(g.last_feed(), Some([0, 0, 0]));
        g.step(&mut ui, &facts(), 0x0004);
        assert_eq!(
            g.last_feed(),
            Some([0x0004, 0, 0x0004]),
            "返した後は語をそのまま"
        );
    }

    #[test]
    fn only_the_first_create_or_paste_asks_to_open_the_net() {
        let (mut ui, link, _b) = new_ui();
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        g.step(&mut ui, &facts(), DOWN);
        g.step(&mut ui, &facts(), 0);
        let o = g.step(&mut ui, &facts(), Z);
        assert!(o.open_net, "ネットワークが開いていなければ貼るでも開く");
        assert_eq!(link.take_order(), Some(Order::Paste));
        g.step(&mut ui, &facts(), 0);
        let open = UiFacts {
            net: NetPhase::Open,
            ..facts()
        };
        let o = g.step(&mut ui, &open, Z);
        assert!(!o.open_net, "ネットワークが開いていれば頼まない");
        assert_eq!(link.take_order(), Some(Order::Paste));
    }

    /// 開いたメニューで「ランダムマッチ」から対戦の「ルームマッチ」まで下へ1つ動かす。
    fn down_to_play(ui: &mut NetUi, g: &mut Fake) {
        g.step(ui, &facts(), DOWN);
        g.step(ui, &facts(), 0);
    }

    /// 開いたメニューで対戦の「ルームマッチ」から観戦の「ルームマッチ」まで下へ3つ動かす（間に「ルーム名」と観戦の「ランダムマッチ」の行がある）。
    fn down_to_watch(ui: &mut NetUi, g: &mut Fake) {
        for _ in 0..3 {
            g.step(ui, &facts(), DOWN);
            g.step(ui, &facts(), 0);
        }
    }

    /// ルーム名の入力を開く（画面6でメニューを開いて対戦の「ルームマッチ」を決定する）。
    /// ルーム名が設定済みだと入力を経ずに始まるので、ルーム名は空にする。決定のキーを離すところまで進める。
    fn open_room_input() -> (NetUi, Arc<UiLink>, &'static Board, Fake) {
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui =
            NetUi::new(ini(), Arc::clone(&link), b).with_room(RoomItem::On(RoomBuf::new()));
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        down_to_play(&mut ui, &mut g);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        (ui, link, b, g)
    }

    #[test]
    fn the_room_item_reaches_the_watcher_and_its_wait_shows_on_every_scene() {
        let (mut ui, _link, b) = new_ui();
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        assert!(
            !menu_rows(b)
                .iter()
                .any(|l| l.contains("ルームマッチ") || l.contains("ランダムマッチ")),
            "ini に room_server が無いのに「ルームマッチ」「ランダムマッチ」が出た"
        );
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b)
            .with_room(RoomItem::On(RoomBuf::from_name("first").unwrap()));
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        let rows = menu_rows(b);
        assert_eq!(rows[0], "対戦", "{rows:?}");
        assert_eq!(rows[1], "> ランダムマッチ", "{rows:?}");
        assert_eq!(rows[2], "  ルームマッチ", "{rows:?}");
        assert_eq!(rows[3], "  ルーム名\u{3000}first", "{rows:?}");
        assert_eq!(rows[5], "観戦", "{rows:?}");
        assert_eq!(rows[6], "  ランダムマッチ");
        assert_eq!(rows[7], "  ルームマッチ");
        assert_eq!(rows[10], "  対戦コード作成");
        down_to_play(&mut ui, &mut g);
        assert_eq!(menu_rows(b)[2], "> ルームマッチ");
        let o = g.step(&mut ui, &facts(), Z);
        assert!(
            o.open_net,
            "決まった名で始める対戦の「ルームマッチ」がネットワークを開く頼みにならない"
        );
        assert_eq!(link.take_order(), Some(Order::Room));
        assert_eq!(link.room_name(), RoomBuf::from_name("first"));
        assert!(
            g.gate_calls.is_empty(),
            "名が決まっているのに入力のゲートを開けた"
        );
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui =
            NetUi::new(ini(), Arc::clone(&link), b).with_room(RoomItem::On(RoomBuf::new()));
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        down_to_play(&mut ui, &mut g);
        assert_eq!(menu_rows(b)[2], "> ルームマッチ");
        let o = g.step(&mut ui, &facts(), Z);
        assert!(!o.open_net, "名を決める前にネットワークを開く頼みが出た");
        assert_eq!(link.take_order(), None, "入力を経ずに指示が出た");
        assert_eq!(menu_rows(b)[0], "ルームマッチ");
        assert_eq!(menu_rows(b)[2], "ルーム名: |");
        assert_eq!(g.gate_calls, vec![true], "入力に入ったのにゲートを開けない");
        g.step(&mut ui, &facts(), 0);
        g.typed.extend("first-2".encode_utf16().chain(Some(0x0D)));
        let o = g.step(&mut ui, &facts(), 0);
        assert!(
            o.open_net,
            "最初の対戦のルームマッチがネットワークを開く頼みにならない"
        );
        assert_eq!(link.take_order(), Some(Order::Room));
        assert_eq!(link.room_name(), RoomBuf::from_name("first-2"));
        assert_eq!(
            g.gate_calls,
            vec![true, false],
            "決めたのにゲートを閉じない"
        );
        g.step(&mut ui, &facts(), 0);
        link.publish(&Note {
            seq: 1,
            event: Event::RoomWaiting,
            state: CodeState::RoomWaiting,
            room: higanr_core::net_menu::sanitize_room("Synth", |_| true),
            since: 1_000_000 - 61,
            ..Note::default()
        });
        g.step(&mut ui, &facts(), X);
        g.step(&mut ui, &facts(), 0);
        assert!(!ui.menu_active());
        g.scenes = (2, 0);
        g.step(&mut ui, &facts(), 0);
        assert_ne!(b.show() & SHOW_WAIT, 0);
        assert_eq!(row_text(b, ROW_WAIT), "ルーム Synth で待機中 1:01");
        assert_eq!(b.show() & (SHOW_TL0 | SHOW_TL1), 0, "2 行目が出た");
    }

    #[test]
    fn the_random_match_item_opens_the_net_and_reaches_the_watcher_without_a_room_name() {
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui =
            NetUi::new(ini(), Arc::clone(&link), b).with_room(RoomItem::On(RoomBuf::new()));
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(menu_rows(b)[1], "> ランダムマッチ", "開いたときのカーソル");
        let o = g.step(&mut ui, &facts(), Z);
        assert!(
            o.open_net,
            "ランダムマッチがネットワークを開く頼みにならない"
        );
        assert!(!o.spectate_open);
        assert_eq!(link.take_order(), Some(Order::Random));
        assert_eq!(link.take_order(), None);
        assert_eq!(
            link.room_name(),
            None,
            "ランダムマッチがルーム名の入れ物に触った"
        );
        assert_eq!(link.spectate_name(), None);
        assert!(g.gate_calls.is_empty(), "ルーム名を打つゲートを開けた");
        assert!(!ui.menu.wants_text());
        g.step(&mut ui, &facts(), 0);
        // ネットワークが開いた後の待機中: 左上の行はルーム名なし、メニューの「ランダムマッチ」は選び直せる。
        link.publish(&Note {
            seq: 1,
            event: Event::RoomWaiting,
            state: CodeState::RoomWaiting,
            random: true,
            since: 1_000_000 - 61,
            ..Note::default()
        });
        g.step(&mut ui, &facts(), 0);
        let rows = menu_rows(b);
        assert!(
            rows.contains(&"ランダムマッチで待機中 1:01".to_string()),
            "{rows:?}"
        );
        assert!(
            rows.contains(&"> ランダムマッチ  待機中 1:01".to_string()),
            "{rows:?}"
        );
        assert!(rows.iter().any(|r| r == "  待機をやめる"), "{rows:?}");
        let open = UiFacts {
            net: NetPhase::Open,
            ..facts()
        };
        let o = g.step(&mut ui, &open, Z);
        assert!(!o.open_net, "ネットワークが開いているのに頼んだ");
        assert_eq!(link.take_order(), Some(Order::Random), "待機中の選び直し");
        g.step(&mut ui, &open, 0);
        g.step(&mut ui, &open, X);
        g.step(&mut ui, &open, 0);
        assert!(!ui.menu_active());
        g.scenes = (2, 0);
        g.step(&mut ui, &open, 0);
        assert_ne!(b.show() & SHOW_WAIT, 0);
        assert_eq!(row_text(b, ROW_WAIT), "ランダムマッチで待機中 1:01");
        assert_eq!(b.show() & (SHOW_TL0 | SHOW_TL1), 0, "2 行目が出た");
        // ルームサーバーが無ければ項目そのものが出ない。
        let (mut ui, _link, b) = new_ui();
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        assert!(!menu_rows(b).iter().any(|r| r.contains("ランダムマッチ")));
    }

    fn watch_row(i: u8, p1: &str, p2: &str, watchers: u8) -> higanr_core::watch_list::WatchRow {
        use higanr_core::watch_list::{WatchName, WatchRow};
        WatchRow {
            hash: [i; 8],
            p1: WatchName::parse(p1).unwrap(),
            p2: WatchName::parse(p2).unwrap(),
            mode: Mode::Rollback,
            delay: 1,
            watchers,
            cap: 10,
        }
    }

    #[test]
    fn the_spectate_item_reaches_the_watcher_without_opening_the_net() {
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b)
            .with_room(RoomItem::On(RoomBuf::from_name("first").unwrap()));
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        let rows = menu_rows(b);
        assert_eq!(rows[2], "  ルームマッチ", "{rows:?}");
        assert_eq!(rows[3], "  ルーム名\u{3000}first", "{rows:?}");
        assert_eq!(rows[5], "観戦", "{rows:?}");
        assert_eq!(rows[6], "  ランダムマッチ", "{rows:?}");
        assert_eq!(rows[7], "  ルームマッチ", "{rows:?}");
        down_to_play(&mut ui, &mut g);
        down_to_watch(&mut ui, &mut g);
        let o = g.step(&mut ui, &facts(), Z);
        assert!(!o.open_net, "観戦の決定がネットワークを開く頼みになった");
        assert!(
            o.spectate_open,
            "決まった名で始める観戦の「ルームマッチ」が観戦者のネットワークを開く頼みにならない"
        );
        assert_eq!(link.take_order(), Some(Order::Spectate));
        assert_eq!(link.spectate_name(), RoomBuf::from_name("first"));
        assert_eq!(link.room_name(), None, "観戦の名が対戦の入れ物へ届いた");
        assert!(
            g.gate_calls.is_empty(),
            "名が決まっているのに入力のゲートを開けた"
        );
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui =
            NetUi::new(ini(), Arc::clone(&link), b).with_room(RoomItem::On(RoomBuf::new()));
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        down_to_play(&mut ui, &mut g);
        down_to_watch(&mut ui, &mut g);
        let o = g.step(&mut ui, &facts(), Z);
        assert!(!o.open_net, "名を決める前にネットワークを開く頼みが出た");
        assert!(
            !o.spectate_open,
            "名を決める前に観戦者のネットワークを開く頼みが出た"
        );
        assert_eq!(link.take_order(), None, "入力を経ずに指示が出た");
        let rows = menu_rows(b);
        assert_eq!(
            (rows[0].as_str(), rows[2].as_str()),
            ("観戦", "ルーム名: |"),
            "{rows:?}"
        );
        assert_eq!(g.gate_calls, vec![true], "入力に入ったのにゲートを開けない");
        g.step(&mut ui, &facts(), 0);
        g.typed.extend("first-2".encode_utf16().chain(Some(0x0D)));
        let o = g.step(&mut ui, &facts(), 0);
        assert!(!o.open_net, "観戦の決定がネットワークを開く頼みになった");
        assert!(
            o.spectate_open,
            "観戦の決定が観戦者のネットワークを開く頼みにならない"
        );
        assert_eq!(link.take_order(), Some(Order::Spectate));
        assert_eq!(link.take_order(), None);
        assert_eq!(link.spectate_name(), RoomBuf::from_name("first-2"));
        assert_eq!(
            link.room_name(),
            None,
            "観戦の名が対戦のルームマッチの入れ物へ届いた"
        );
        assert_eq!(
            g.gate_calls,
            vec![true, false],
            "決めたのにゲートを閉じない"
        );
        g.step(&mut ui, &facts(), 0);
        link.publish(&Note {
            seq: 1,
            event: Event::RoomWaiting,
            state: CodeState::SpectateWaiting,
            room: higanr_core::net_menu::sanitize_room("Synth", |_| true),
            since: 1_000_000 - 61,
            ..Note::default()
        });
        g.step(&mut ui, &facts(), 0);
        let rows = menu_rows(b);
        assert_eq!(rows[0], "> 観戦をやめる", "{rows:?}");
        assert_eq!(rows[1], "", "項目が 2 つ以上並んだ: {rows:?}");
        assert!(
            rows.contains(&"観戦 Synth で待機中 1:01".to_string()),
            "{rows:?}"
        );
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(link.take_order(), Some(Order::SpectateCancel));
        assert_eq!(link.take_order(), None, "対戦者の取りやめの指示が出た");
        g.step(&mut ui, &facts(), X);
        g.step(&mut ui, &facts(), 0);
        assert!(!ui.menu_active());
        g.scenes = (2, 0);
        g.step(&mut ui, &facts(), 0);
        assert_ne!(b.show() & SHOW_WAIT, 0);
        assert_eq!(row_text(b, ROW_WAIT), "観戦 Synth で待機中 1:01");
        assert_eq!(b.show() & (SHOW_TL0 | SHOW_TL1), 0, "2 行目が出た");
        link.publish(&Note {
            seq: 2,
            event: Event::SpectateGaveUp,
            state: CodeState::SpectateIdle,
            ..Note::default()
        });
        g.step(&mut ui, &facts(), 0);
        assert_eq!(b.show() & SHOW_WAIT, 0, "待ちの行が残った");
        assert_eq!(
            row_text(b, ROW_TL0),
            "30分経ちましたが相手がルームに来ませんでした。"
        );
        assert_eq!(
            row_text(b, ROW_TL1),
            "ルーム名をチェックして、観戦の「ルームマッチ」を選び直してください。"
        );
    }

    #[test]
    fn only_the_spectate_decision_asks_for_a_watcher_net_and_the_room_decision_for_a_player_net() {
        let (mut ui, link, _b, mut g) = open_room_input();
        let o = g.step(&mut ui, &facts(), 0);
        assert_eq!(
            (o.open_net, o.spectate_open),
            (false, false),
            "入力の間に頼んだ"
        );
        g.typed.extend("first".encode_utf16().chain(Some(0x0D)));
        let o = g.step(&mut ui, &facts(), 0);
        assert_eq!(
            (o.open_net, o.spectate_open),
            (true, false),
            "対戦の「ルームマッチ」は対戦者のネットワークを開く頼みだけ"
        );
        assert_eq!(link.take_order(), Some(Order::Room));
    }

    #[test]
    fn the_watcher_walks_screen_six_to_seven_only_while_it_receives_the_stream() {
        let (mut ui, _l, _b) = new_ui();
        let mut g = Fake::on_match_screen(2);
        g.step(&mut ui, &facts(), 0);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(g.feeds, vec![[0, 0, 0], [0, 0, 0]], "受ける前に押した");
        ui.set_watch_advance(true);
        let mut cursor = 2i32;
        let mut decided_at = None;
        let mut presses = Vec::new();
        for _ in 0..40 {
            g.view.as_mut().unwrap().cursor = cursor as u32;
            let before = g.im2;
            g.step(&mut ui, &facts(), 0);
            let w = g.last_feed().unwrap();
            presses.push(w[0]);
            let (pressed, repeat) = ui_entry::predicted_edges(&before, w[2]);
            cursor = ui_entry::cursor_after_move(cursor, pressed | repeat);
            if pressed & DECIDE_BITS != 0 {
                decided_at = Some(cursor);
                break;
            }
        }
        assert_eq!(decided_at, Some(0), "決定はカーソル 0 で（{presses:?}）");
        assert_eq!(ui.stats().auto_presses, 3, "上 2 回 + 決定 1 回");
        let mut g7 = Fake::on_screen(7, 0);
        let o = g7.step(&mut ui, &facts(), 0);
        assert_eq!(o.action, None);
        assert_eq!(g7.stash_calls, 0, "観戦者が語を預けた");
        let mut g6 = Fake::on_match_screen(2);
        ui.set_watch_advance(true);
        g6.step(&mut ui, &facts(), X);
        assert_eq!(ui.stats().confirms, 0, "観戦者の取消で確かめの枠が開いた");
        ui.after_watch_teardown();
        let before = ui.stats().auto_presses;
        let mut g6b = Fake::on_match_screen(3);
        for _ in 0..4 {
            g6b.step(&mut ui, &facts(), 0);
        }
        assert_eq!(ui.stats().auto_presses, before, "手放した後も前進した");
        assert_eq!(ui.stats().teardowns, 1);
    }

    #[test]
    fn after_a_watcher_box_is_released_the_end_reason_stays_and_the_items_come_back() {
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b)
            .with_room(RoomItem::On(RoomBuf::from_name("first").unwrap()));
        let mut g = Fake::on_match_screen(2);
        link.publish(&Note {
            seq: 5,
            event: Event::SpectateEnded,
            state: CodeState::Idle,
            ..Note::default()
        });
        g.step(&mut ui, &facts(), 0);
        ui.after_watch_teardown();
        g.step(&mut ui, &facts(), 0);
        assert_eq!(
            row_text(b, ROW_TL0),
            "観戦を終えました。",
            "終わりの理由が消えた"
        );
        let mut g4 = Fake::on_match_screen(4);
        g4.step(&mut ui, &facts(), Z);
        g4.step(&mut ui, &facts(), 0);
        let rows = menu_rows(b);
        assert!(
            rows.iter().any(|r| r == "> ランダムマッチ")
                && rows.iter().any(|r| r == "  ルームマッチ")
                && rows.iter().any(|r| r == "観戦")
                && rows.iter().any(|r| r == "  対戦コード作成"),
            "{rows:?}"
        );
    }

    /// 観戦者の経路の用意: 本物の`WatchPlay`と仮のネットワーク（相手は居ない）。
    /// コードは設定済み（`pump`が`Watcher`を作って観戦のホストへJOINを送り始める）。
    fn watch_rig() -> (
        crate::spectate_watch::WatchPlay,
        crate::netplay::fake::FakeNet,
    ) {
        use crate::spectate_watch::{WATCH_MARGIN_FRAMES, WatchConfig, WatchPlay, WatchSlot};
        use higanr_core::candidate::Candidates;
        use higanr_core::handshake::Name;
        use higanr_core::spectate::WatcherConfig;
        let a = std::net::SocketAddr::from(([127, 0, 0, 1], 41_201));
        let h = std::net::SocketAddr::from(([127, 0, 0, 1], 41_202));
        let (net, _other, _clock) = crate::netplay::fake::FakeNet::pair(a, h, 0);
        let slot = Arc::new(WatchSlot::new());
        slot.offer(WatcherConfig {
            session_id: 0x77,
            secret: higanr_core::auth::Secret([3; 16]),
            nonce: 1,
            name: Name::new("w"),
            host: Candidates::from_addrs(&[h]).unwrap(),
            host_public: higanr_core::seal::Ephemeral::from_seed(&higanr_core::seal::KeySeed(
                [0x77; 32],
            ))
            .public(),
            key_seed: higanr_core::seal::KeySeed([0x57; 32]),
        });
        let wp = WatchPlay::new(
            slot,
            WatchConfig {
                wait_budget_us: 16_000,
                margin_frames: WATCH_MARGIN_FRAMES,
                require_net_selection: true,
                ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
            },
        );
        (wp, net)
    }

    #[test]
    fn a_watcher_box_keeps_the_entry_check_and_keeps_tending_the_net_while_the_menu_holds() {
        use crate::netplay::Phase;
        let (mut ui, _l, b) = new_ui();
        let (mut wp, mut net) = watch_rig();
        let mut g = Fake::on_match_screen(4);
        g.words.push_back(Z);
        g.cur_feed = [None; 3];
        let (a, out) = watch_input(&mut ui, &mut g, &mut wp, &mut net, 1_000_000);
        assert_eq!(a, Action::Menu);
        assert_eq!(out.action, Some(Action::Menu));
        assert_eq!(
            [g.cur_feed[0], g.cur_feed[1], g.cur_feed[2]],
            [Some(0), Some(0), Some(0)],
            "決定が漏れた"
        );
        assert!(ui.menu_active(), "メニューが開いていない");
        assert_eq!(wp.phase(), Phase::Unconnected);
        let sent0 = net.sent;
        for _ in 0..30 {
            g.cur_feed = [None; 3];
            let (a, _) = watch_input(&mut ui, &mut g, &mut wp, &mut net, 1_000_000);
            assert_eq!(a, Action::Menu);
            net.wait(20_000);
        }
        assert!(
            net.sent > sent0,
            "メニューが握る tick にネットワークの手入れが止まった"
        );
        assert!(
            menu_rows(b).iter().any(|r| r.contains("コード作成")),
            "{:?}",
            menu_rows(b)
        );
        // 画面7は占有しない（観戦者の判断。接続する前は自分の入力を通す）。メニューを開いていないセッションで見る（開いているメニューは画面7でも占有する）。
        let (mut ui, _l, _b) = new_ui();
        let mut g7 = Fake::on_screen(7, 0);
        g7.cur_feed = [None; 3];
        let (a, out) = watch_input(&mut ui, &mut g7, &mut wp, &mut net, 1_000_000);
        assert_eq!(out.action, None);
        assert_eq!(a, Action::Menu, "繋がる前の画面 7 は決定を落として通す");
    }

    #[test]
    fn the_watch_input_path_feeds_the_match_bands_only_inside_a_match() {
        use crate::netplay::Phase;
        let (mut ui, _l, b) = new_ui();
        let (mut wp, mut net) = watch_rig();
        let mut g = Fake::on_screen(7, 0);
        wp.set_names_for_test("synth-one", "synth-two");
        wp.set_phase_for_test(Phase::Selecting);
        g.cur_feed = [None; 3];
        let _ = watch_input(&mut ui, &mut g, &mut wp, &mut net, 1_000_000);
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), 0);
        wp.set_phase_for_test(Phase::Lockstep);
        g.cur_feed = [None; 3];
        let _ = watch_input(&mut ui, &mut g, &mut wp, &mut net, 1_000_000);
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), SHOW_HUD | SHOW_NAMES);
        assert_eq!(row_text(b, ROW_NAME_L), "1P synth-one");
        assert_eq!(row_text(b, ROW_NAME_R), "synth-two 2P");
        assert!(row_text(b, ROW_HUD).starts_with("Watch delay "));
        wp.set_phase_for_test(Phase::Ended);
        g.cur_feed = [None; 3];
        let _ = watch_input(&mut ui, &mut g, &mut wp, &mut net, 1_000_000);
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), 0);
        let (mut ui, _l, b) = new_ui();
        let (mut wp, mut net) = watch_rig();
        wp.set_phase_for_test(Phase::Lockstep);
        g.cur_feed = [None; 3];
        let _ = watch_input(&mut ui, &mut g, &mut wp, &mut net, 1_000_000);
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), SHOW_HUD);
    }

    #[test]
    fn a_watcher_leaves_by_holding_esc_for_three_seconds_on_screen_six_unless_the_menu_was_the_one_closing()
     {
        use crate::netplay::{Net, Phase};
        fn tick(
            ui: &mut NetUi,
            g: &mut Fake,
            wp: &mut crate::spectate_watch::WatchPlay,
            net: &mut crate::netplay::fake::FakeNet,
        ) -> (Action, UiOut) {
            g.cur_feed = [None; 3];
            // 1 tickごとに仮の時計を100 ms進める（届くパケットは無いので待ちの長さだけ進む）。
            net.wait(100_000);
            watch_input(ui, g, wp, net, 1_000_000)
        }
        let (mut ui, _l, b) = new_ui();
        let (mut wp, mut net) = watch_rig();
        wp.set_phase_for_test(Phase::Selecting);
        let mut g = Fake::on_match_screen(2);
        // 押しっぱなしで接続したときは離すまで長押しを数えない。
        g.esc_word = ESC;
        for _ in 0..40 {
            let _ = tick(&mut ui, &mut g, &mut wp, &mut net);
        }
        assert_eq!(wp.end(), None, "押しっぱなしで抜けた");
        g.esc_word = 0;
        let _ = tick(&mut ui, &mut g, &mut wp, &mut net);
        assert_eq!(b.show() & SHOW_PEER_WAIT, 0, "押す前に案内が出た");
        // 2.9秒で離すと終わらない。押している間と離して3秒は案内を出す。
        g.esc_word = ESC;
        for _ in 0..30 {
            let _ = tick(&mut ui, &mut g, &mut wp, &mut net);
            assert_eq!(row_text(b, ROW_PEER_WAIT), T_SPECTATE_ESC_HOLD.ja);
        }
        assert_eq!(b.show() & SHOW_PEER_WAIT, SHOW_PEER_WAIT);
        assert_eq!(wp.end(), None, "2.9 秒で抜けた");
        g.esc_word = 0;
        for _ in 0..30 {
            let _ = tick(&mut ui, &mut g, &mut wp, &mut net);
        }
        assert_eq!(
            b.show() & SHOW_PEER_WAIT,
            SHOW_PEER_WAIT,
            "離して 3 秒より前に消えた"
        );
        let _ = tick(&mut ui, &mut g, &mut wp, &mut net);
        assert_eq!(b.show() & SHOW_PEER_WAIT, 0, "離して 3 秒たっても残った");
        assert_eq!(wp.end(), None);
        // 3秒押し続けると、メニューが握るtickでも終える。
        g.esc_word = ESC;
        for _ in 0..30 {
            let _ = tick(&mut ui, &mut g, &mut wp, &mut net);
            assert_eq!(wp.end(), None, "3 秒より前に抜けた");
        }
        let (a, out) = tick(&mut ui, &mut g, &mut wp, &mut net);
        assert_eq!(wp.end(), Some(crate::spectate_watch::WatchEnd::Quit));
        assert_eq!(b.show() & SHOW_PEER_WAIT, 0, "終えた後も案内が残った");
        // 画面6は占有する（自動の前進のtickはメニューが0や上・決定をゲームへ渡すので、このtickのEscはゲームへ届かない）。
        assert_eq!(out.action, Some(Action::Menu));
        assert_eq!(a, Action::Menu);
        g.words.push_back(ESC);
        let _ = tick(&mut ui, &mut g, &mut wp, &mut net);
        assert_ne!(
            g.cur_feed[0].unwrap_or(0) & ESC,
            0,
            "やめた後の Esc がゲームへ届かない"
        );

        // 開いているメニュー（観戦の「ルームマッチ」を決定したメニューは待ちの間ずっと開いている）は、ホストの流れを受け始めたtickに閉じる。
        // 閉じると自動の前進が画面6を進める（開いたままだとメニューが入力を占有して前進が走らず、画面6に留まる）。
        // 閉じる前のEscはメニューを閉じるだけで観戦は続く。
        let (mut ui, _l, _b) = new_ui();
        let (mut wp, mut net) = watch_rig();
        let mut g = Fake::on_match_screen(4);
        g.words.push_back(Z);
        let _ = tick(&mut ui, &mut g, &mut wp, &mut net);
        assert!(ui.menu_active());
        g.esc_word = 0;
        let _ = tick(&mut ui, &mut g, &mut wp, &mut net);
        g.esc_word = ESC;
        g.words.push_back(ESC);
        let _ = tick(&mut ui, &mut g, &mut wp, &mut net);
        assert_eq!(wp.end(), None, "メニューを閉じる Esc で観戦まで終わった");
        let (mut ui, _l, _b) = new_ui();
        let (mut wp, mut net) = watch_rig();
        let mut g = Fake::on_match_screen(4);
        g.words.push_back(Z);
        let _ = tick(&mut ui, &mut g, &mut wp, &mut net);
        assert!(ui.menu_active());
        g.esc_word = 0;
        wp.set_phase_for_test(Phase::Selecting);
        let presses0 = ui.stats().auto_presses;
        let _ = tick(&mut ui, &mut g, &mut wp, &mut net);
        assert!(
            !ui.menu_active(),
            "流れを受け始めてもメニューが閉じない（自動の前進が走らない）"
        );
        assert_eq!(
            ui.stats().auto_presses,
            presses0 + 1,
            "閉じた tick に自動の前進が押していない"
        );
        assert_eq!(wp.end(), None);

        let (mut ui, _l, _b) = new_ui();
        let (mut wp, mut net) = watch_rig();
        wp.set_phase_for_test(Phase::Selecting);
        let mut g = Fake::on_screen(7, 0);
        g.esc_word = 0;
        let _ = tick(&mut ui, &mut g, &mut wp, &mut net);
        g.esc_word = ESC;
        for _ in 0..30 {
            let (_, out) = tick(&mut ui, &mut g, &mut wp, &mut net);
            assert_eq!(out.action, None, "画面 7 でメニューが握った");
            assert_eq!(wp.end(), None, "画面 7 で 3 秒より前に抜けた");
        }
        let (a, out) = tick(&mut ui, &mut g, &mut wp, &mut net);
        assert_eq!(out.action, None, "画面 7 でメニューが握った");
        assert_eq!(a, Action::Local);
        assert_eq!(wp.end(), Some(crate::spectate_watch::WatchEnd::Quit));
    }

    #[test]
    fn while_typing_the_game_gets_zeros_and_keys_are_letters() {
        let (mut ui, link, b, mut g) = open_room_input();
        assert_eq!(g.gate_calls, vec![true]);
        let fed_before = g.feeds.len();
        g.typed.extend("zx".encode_utf16());
        for w in [Z, X, DOWN, UP, 0x1000, Z | X] {
            g.step(&mut ui, &facts(), w);
        }
        assert_eq!(menu_rows(b)[2], "ルーム名: zx|");
        assert!(
            g.feeds[fed_before..].iter().all(|f| *f == [0, 0, 0]),
            "入力の間にゲームへ語が届いた: {:?}",
            &g.feeds[fed_before..]
        );
        assert_eq!(link.take_order(), None, "文字の z・x が指示になった");
        assert_eq!(g.gate_calls, vec![true], "文字の z・x で入力が閉じた");
        g.step(&mut ui, &facts(), ESC);
        assert_eq!(g.gate_calls, vec![true, false]);
        assert!(ui.menu_active() && !ui.menu.wants_text());
        let calls = g.typed_while_closed;
        g.typed.extend("late".encode_utf16());
        for _ in 0..5 {
            g.step(&mut ui, &facts(), 0);
        }
        assert_eq!(
            g.typed_while_closed, calls,
            "ゲートが閉じている間に文字を取りに行った"
        );
        assert_eq!(g.typed.len(), 4, "閉じている間の文字を取った");
        g.typed.clear();
        g.step(&mut ui, &facts(), Z);
        assert_eq!(g.gate_calls, vec![true, false, true]);
        assert_eq!(menu_rows(b)[2], "ルーム名: |", "やめた入力の文字が残った");
    }

    #[test]
    fn when_the_gate_cannot_open_the_menu_says_so_and_esc_still_leaves() {
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui =
            NetUi::new(ini(), Arc::clone(&link), b).with_room(RoomItem::On(RoomBuf::new()));
        let mut g = Fake::on_match_screen(4);
        g.gate_ok = false;
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        down_to_play(&mut ui, &mut g);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        assert!(ui.menu.wants_text());
        let rows = menu_rows(b);
        assert!(
            rows.iter()
                .any(|l| l == higanr_core::net_menu::T_ROOM_INPUT_NO_KEYS.ja),
            "{rows:?}"
        );
        g.typed.extend("abc".encode_utf16());
        g.step(&mut ui, &facts(), 0);
        assert_eq!(g.typed.len(), 3, "開けられなかったゲートから文字を取った");
        assert_eq!(
            g.typed_while_closed, 0,
            "開けられなかった（閉じている）ゲートへ文字を取りに行った"
        );
        assert_eq!(menu_rows(b)[2], "ルーム名: |");
        g.step(&mut ui, &facts(), ESC);
        assert!(!ui.menu.wants_text(), "Esc で戻れない");
        // 開けられなかったゲートは閉じる必要が無い。開けようとした呼び出しだけが並ぶ。開けようとするのは入力1回につき1回（ウィンドウの探し直しは重いので毎tickやらない）。
        assert_eq!(g.gate_calls, vec![true], "入力の間に毎 tick 開け直した");
        g.step(&mut ui, &facts(), 0);
        g.gate_ok = true;
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        assert!(ui.menu.wants_text());
        assert_eq!(g.gate_calls, vec![true, true], "次の入力で探し直さない");
        assert!(g.gate_open, "探し直してもゲートが開かない");
        let rows = menu_rows(b);
        assert!(
            !rows
                .iter()
                .any(|l| l == higanr_core::net_menu::T_ROOM_INPUT_NO_KEYS.ja),
            "開けたのに断りの行が残った: {rows:?}"
        );
    }

    #[test]
    fn the_room_name_box_round_trips_and_refuses_half_written() {
        let nb = RoomNameBox::new();
        assert_eq!(nb.read(), None, "まだ書かれていない");
        let a = RoomBuf::from_name("Alpha-1").unwrap();
        nb.publish(&a);
        assert_eq!(nb.read(), Some(a));
        let b = RoomBuf::from_name("b").unwrap();
        nb.publish(&b);
        assert_eq!(nb.read(), Some(b), "新しい名が古い名を置き換えない");
        nb.publish(&RoomBuf::new());
        assert_eq!(nb.read(), Some(RoomBuf::new()));
        nb.seq.fetch_add(1, Ordering::SeqCst);
        assert_eq!(nb.read(), None);
    }

    #[test]
    fn the_board_shows_the_menu_then_the_corner_line_after_closing() {
        let (mut ui, link, b) = new_ui();
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        assert_ne!(b.show() & SHOW_MENU, 0);
        assert_eq!(row_text(b, 0), "接続コード");
        assert!(row_text(b, 1).contains("対戦コード作成"));
        let opening = UiFacts {
            net: NetPhase::Opening,
            ..facts()
        };
        g.step(&mut ui, &opening, 0);
        assert!(
            menu_rows(b).iter().any(|l| l.contains("準備")),
            "{:?}",
            menu_rows(b)
        );
        link.publish(&Note {
            seq: 1,
            event: Event::Made,
            state: CodeState::Offered,
            code_len: 58,
            head: *b"AbCd",
            tail: *b"QrSt",
            since: 1_000_000 - 42,
            ..Note::default()
        });
        g.step(&mut ui, &facts(), 0);
        let all = menu_rows(b);
        assert!(all.iter().any(|l| l == "AbCd…QrSt（58 文字）"), "{all:?}");
        assert!(!all.iter().any(|l| l.contains("準備")), "開いた後は消える");
        g.step(&mut ui, &facts(), ESC);
        g.step(&mut ui, &facts(), 0);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(b.show() & SHOW_MENU, 0);
        assert_ne!(b.show() & SHOW_WAIT, 0);
        assert_eq!(row_text(b, ROW_WAIT), "接続コードで待機中 0:42 残り 29:18");
        assert_eq!(b.show() & (SHOW_TL0 | SHOW_TL1), 0, "2 行目が出た");
        g.scenes = (2, 0);
        g.step(&mut ui, &connected_facts(), 0);
        assert_eq!(b.show() & (SHOW_TL0 | SHOW_TL1 | SHOW_MENU | SHOW_WAIT), 0);
    }

    #[test]
    fn nothing_is_drawn_or_requested_before_the_menu_is_used() {
        let (mut ui, link, b) = new_ui();
        let mut out = UiOut::default();
        for cursor in 0..4 {
            let mut g = Fake::on_match_screen(cursor);
            for w in [0, DOWN, 0, UP, 0, Z, 0] {
                let o = g.step(&mut ui, &facts(), w);
                out.open_net |= o.open_net;
                out.leave |= o.leave;
            }
        }
        assert!(!out.open_net && !out.leave);
        assert_eq!(b.show(), 0, "何も描かない");
        assert_eq!(link.take_order(), None);
        assert_eq!(ui.stats().opened, 0);
    }

    #[test]
    fn after_connecting_it_walks_the_match_cursor_up_and_decides_at_zero_only() {
        let (mut ui, _l, _b) = new_ui();
        let mut g = Fake::on_match_screen(2);
        g.step(&mut ui, &facts(), 0);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(g.feeds, vec![[0, 0, 0], [0, 0, 0]]);
        let c = connected_facts();
        let mut cursor = 2i32;
        let mut decided_at = None;
        let mut presses = Vec::new();
        for _ in 0..40 {
            g.view.as_mut().unwrap().cursor = cursor as u32;
            let before = g.im2;
            g.step(&mut ui, &c, 0);
            let w = g.last_feed().unwrap();
            assert_eq!(w[1], 0);
            assert_eq!(w[0], w[2]);
            presses.push(w[0]);
            let (pressed, repeat) = ui_entry::predicted_edges(&before, w[2]);
            cursor = ui_entry::cursor_after_move(cursor, pressed | repeat);
            if pressed & DECIDE_BITS != 0 {
                decided_at = Some(cursor);
                break;
            }
        }
        assert_eq!(decided_at, Some(0), "決定はカーソル 0 で（{presses:?}）");
        assert_eq!(presses.iter().filter(|&&w| w & DECIDE_BITS != 0).count(), 1);
        assert_eq!(ui.stats().auto_presses, 3, "上 2 回 + 決定 1 回");
        let mut g7 = Fake::on_screen(7, 0);
        let o = g7.step(&mut ui, &c, 0);
        assert_eq!(o.action, None);
        assert_eq!(g7.stash_calls, 1);
        let mut g6 = Fake::on_match_screen(2);
        g6.step(&mut ui, &c, 0);
        assert_eq!(
            g6.feeds,
            vec![[0, 0, 0]],
            "前進が終わった後の画面 6 では人の語（0）のまま"
        );
        assert_eq!(ui.stats().auto_presses, 3, "増えていない");
    }

    #[test]
    fn a_player_who_was_elsewhere_when_connected_is_not_moved_later() {
        let (mut ui, _l, _b) = new_ui();
        let c = connected_facts();
        let mut title = Fake::on_screen(1, 0);
        for _ in 0..(AUTO_MAX_TICKS * 2) {
            title.step(&mut ui, &c, 0);
        }
        let mut g6 = Fake::on_match_screen(2);
        for _ in 0..30 {
            g6.step(&mut ui, &c, 0);
        }
        assert!(
            g6.feeds.iter().all(|f| *f == [0, 0, 0]),
            "自分で操作していないのに押された: {:?}",
            g6.feeds
        );
        assert_eq!(ui.stats().auto_presses, 0);
        g6.step(&mut ui, &c, DOWN);
        assert_eq!(g6.last_feed(), Some([DOWN, 0, DOWN]));
    }

    #[test]
    fn the_advance_gives_up_after_the_cap_when_the_cursor_never_moves() {
        let (mut ui, _l, _b) = new_ui();
        let c = connected_facts();
        let mut g = Fake::on_match_screen(2);
        let presses_at = |ui: &NetUi| ui.stats().auto_presses;
        let mut at_cap = None;
        for t in 0..(AUTO_MAX_TICKS + 40) {
            g.step(&mut ui, &c, 0);
            if t + 1 == AUTO_MAX_TICKS {
                at_cap = Some(presses_at(&ui));
            }
        }
        let at_cap = at_cap.unwrap();
        // 上を押すtickと離すtickが交互に来るので上限までに押した数は上限の半分ほど（0でも上限いっぱいでもない）。
        assert!(
            at_cap > AUTO_MAX_TICKS / 4 && at_cap <= AUTO_MAX_TICKS / 2 + 1,
            "上限までの押下 {at_cap}"
        );
        assert_eq!(presses_at(&ui), at_cap, "上限の後も押している");
        g.step(&mut ui, &c, DOWN);
        assert_eq!(g.last_feed(), Some([DOWN, 0, DOWN]));
    }

    #[test]
    fn the_second_players_pad_still_works_on_the_match_screen_and_cannot_leak() {
        let (mut ui, _l, _b) = new_ui();
        let mut g = Fake::on_match_screen(3);
        g.dev1 = DOWN;
        let o = g.step(&mut ui, &facts(), 0);
        assert_eq!(o.action, Some(Action::Menu));
        assert_eq!(g.feeds, vec![[0, DOWN, DOWN]]);
        g.dev1 = UP;
        g.step(&mut ui, &facts(), 0x0004);
        assert_eq!(g.last_feed(), Some([0x0004, UP, 0x0004 | UP]));
        // カーソル4で2Pのパッドの決定: 1Pのワードは0でもゲームが読む[2]に決定が立つので奪う（0を3本、メニューが開く）。
        let (mut ui, _l, _b) = new_ui();
        let mut g = Fake::on_match_screen(4);
        g.dev1 = Z;
        g.step(&mut ui, &facts(), 0);
        assert_eq!(g.feeds, vec![[0, 0, 0]], "2P のパッドの決定が漏れた");
        assert!(ui.menu_active());
        assert_eq!(ui.stats().intercepts, 1);
        let (mut ui, _l, _b) = new_ui();
        let mut g = Fake::on_match_screen(3);
        g.dev1 = DOWN | Z;
        g.step(&mut ui, &facts(), 0);
        assert_eq!(g.feeds, vec![[0, 0, 0]], "2P のパッドの 下 ＋ 決定が漏れた");
        let (mut ui, _l, _b) = new_ui();
        let mut g = Fake::on_match_screen(2);
        g.dev1 = Z;
        g.step(&mut ui, &facts(), 0);
        assert_eq!(g.feeds, vec![[0, Z, Z]]);
        assert!(!ui.menu_active());
    }

    #[test]
    fn the_waiting_line_is_shown_on_every_scene_and_only_while_waiting() {
        let waiting_note = Note {
            seq: 1,
            event: Event::Made,
            state: CodeState::Offered,
            code_len: 58,
            head: *b"AbCd",
            tail: *b"QrSt",
            since: 1_000_000 - 42,
            ..Note::default()
        };
        let (mut ui, link, b) = new_ui();
        link.publish(&waiting_note);
        for (scenes, view) in [
            ((2u32, 0u32), true),
            ((SCENE_MENU, SCENE_MATCH), false),
            ((SCENE_MENU, 0), false),
        ] {
            let mut g = Fake::on_match_screen(0);
            g.scenes = scenes;
            if !view {
                g.view = None;
            }
            assert_eq!(g.step(&mut ui, &facts(), 0), UiOut::default());
            assert_ne!(b.show() & SHOW_WAIT, 0, "{scenes:?}: 待っている行が出ない");
            assert_eq!(row_text(b, ROW_WAIT), "接続コードで待機中 0:42 残り 29:18");
            assert_eq!(b.show() & (SHOW_MENU | SHOW_TL0 | SHOW_TL1), 0);
        }
        ui.hud(
            true,
            HudFacts {
                latency: Some(1),
                rtt_ms: Some(30),
                ..HudFacts::default()
            },
        );
        assert_eq!(b.show() & (SHOW_WAIT | SHOW_HUD), SHOW_WAIT | SHOW_HUD);
        const { assert!(TL_Y >= HUD_Y + HUD_BAND_H) };
        let mut g = Fake::on_match_screen(0);
        g.scenes = (2, 0);
        g.step(&mut ui, &connected_facts(), 0);
        assert_eq!(b.show() & (SHOW_TL0 | SHOW_TL1 | SHOW_WAIT), 0);
        assert_ne!(b.show() & SHOW_HUD, 0, "試合中の行は残る");
        let fin = UiFacts {
            conn: Conn::Finished,
            net: NetPhase::Open,
            ..facts()
        };
        g.step(&mut ui, &fin, 0);
        assert_eq!(b.show() & (SHOW_TL0 | SHOW_TL1), 0);
        let (mut ui2, _l2, b2) = new_ui();
        let mut g = Fake::on_match_screen(0);
        g.scenes = (2, 0);
        g.step(&mut ui2, &facts(), 0);
        assert_eq!(b2.show(), 0);
    }

    #[test]
    fn a_cancel_before_the_difficulty_screen_asks_before_leaving() {
        let (mut ui, _l, b) = new_ui();
        let c = connected_facts();
        ui.was_connected = true;
        let mut g = Fake::on_match_screen(0);
        g.step(&mut ui, &c, 0);
        let o = g.step(&mut ui, &c, X);
        assert_eq!(o.action, Some(Action::Menu));
        assert_eq!(g.last_feed(), Some([0, 0, 0]), "取消はゲームへ通さない");
        assert!(ui.menu_active());
        assert_eq!(row_text(b, 0), "対戦から抜けますか？");
        g.step(&mut ui, &c, 0);
        let o = g.step(&mut ui, &c, Z);
        assert!(!o.leave);
        g.step(&mut ui, &c, 0);
        assert!(!ui.menu_active());
        let after_no: Vec<[u16; 3]> = g.feeds.clone();
        assert!(
            after_no.iter().all(|f| f[0] & CANCEL_BITS == 0),
            "取消は通っていない {after_no:?}"
        );
        g.step(&mut ui, &c, X);
        g.step(&mut ui, &c, 0);
        g.step(&mut ui, &c, UP);
        g.step(&mut ui, &c, 0);
        let o = g.step(&mut ui, &c, Z);
        assert!(o.leave, "はい: 対戦を終える");
        assert_eq!(ui.stats().leaves, 1);
        let fin = UiFacts {
            conn: Conn::Finished,
            net: NetPhase::Open,
            ..facts()
        };
        g.step(&mut ui, &fin, 0);
        let before = g.feeds.len();
        let o = g.step(&mut ui, &fin, 0);
        assert_eq!(o.action, Some(Action::Menu));
        assert_eq!(
            g.feeds[before..],
            [[KEY_CANCEL, 0, KEY_CANCEL]],
            "取消が 1 回通る"
        );
        g.step(&mut ui, &fin, 0);
        assert_eq!(g.last_feed(), Some([0, 0, 0]), "2 回目は通らない");
        let (mut ui2, _l2, _b2) = new_ui();
        let mut g = Fake::on_match_screen(0);
        g.step(&mut ui2, &facts(), 0);
        g.step(&mut ui2, &facts(), X);
        assert_eq!(g.last_feed(), Some([X, 0, X]));
        assert!(!ui2.menu_active());
    }

    #[test]
    fn the_difficulty_screen_hands_words_to_the_net_menu_except_a_cancel() {
        let (mut ui, _l, _b) = new_ui();
        let c = connected_facts();
        ui.was_connected = true;
        let mut g = Fake::on_screen(7, 0);
        let o = g.step(&mut ui, &c, DOWN);
        assert_eq!((o.action, g.stashed, g.local_reads), (None, Some(DOWN), 1));
        g.step(&mut ui, &c, 0);
        g.stashed = None;
        let o = g.step(&mut ui, &c, X);
        assert_eq!(o.action, Some(Action::Menu));
        assert_eq!(g.last_feed(), Some([0, 0, 0]));
        assert_eq!(g.stashed, None, "取消の語はネットの選択へ渡さない");
        assert!(ui.menu_active());
        for screen in [8, 16] {
            let (mut ui, _l, _b) = new_ui();
            ui.was_connected = true;
            let mut g = Fake::on_screen(screen, 0);
            let o = g.step(&mut ui, &c, X);
            assert_eq!(
                (o.action, g.local_reads, g.stash_calls),
                (None, 0, 0),
                "screen {screen}"
            );
        }
    }

    #[test]
    fn the_drawn_texts_use_delay_and_button_names() {
        let (mut ui, _l, b) = new_ui();
        let c = connected_facts();
        let mut g = Fake::on_screen(1, 0);
        g.step(&mut ui, &c, 0);
        assert_eq!(
            row_text(b, ROW_TL0),
            "接続しました。あなたは1Pです。（rollback・delay 1）"
        );
        assert_eq!(
            b.show() & SHOW_TL1,
            0,
            "つながった行の下に次にすることは出さない"
        );
        let mut g = Fake::on_match_screen(4);
        let (mut ui, _l, b) = new_ui();
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        open_settings(&mut g, &mut ui, b);
        let rows = menu_rows(b);
        assert!(
            rows.iter()
                .any(|l| l.contains("同期方式\u{3000}◀ rollback ▶")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|l| l.contains("ディレイ\u{3000}◀ 1F ▶")),
            "{rows:?}"
        );
        assert!(
            rows.last()
                .is_some_and(|l| l.contains("決定") && l.contains("キャンセル"))
        );
    }

    #[test]
    fn the_match_line_follows_the_frames() {
        let (mut ui, _l, b) = new_ui();
        assert_eq!(b.show() & SHOW_HUD, 0);
        let h = |rewound| HudFacts {
            latency: Some(1),
            rtt_ms: Some(33),
            rewound,
            recommend: Some(0),
            names: None,
            ..HudFacts::default()
        };
        ui.hud(true, h(Some(0)));
        assert_ne!(b.show() & SHOW_HUD, 0);
        assert_eq!(row_text(b, ROW_HUD), "Delay 1F (0F)  ping 33ms  rollback 0");
        ui.hud(true, h(Some(4)));
        ui.hud(true, h(Some(0)));
        assert_eq!(row_text(b, ROW_HUD), "Delay 1F (0F)  ping 33ms  rollback 4");
        for _ in 0..60 {
            ui.hud(true, h(Some(1)));
        }
        assert_eq!(row_text(b, ROW_HUD), "Delay 1F (0F)  ping 33ms  rollback 1");
        ui.hud(true, h(None));
        assert_eq!(row_text(b, ROW_HUD), "Delay 1F (0F)  ping 33ms");
        assert!(!row_text(b, ROW_HUD).contains('*'));
        ui.hud(false, h(None));
        assert_eq!(b.show() & SHOW_HUD, 0);
    }

    #[test]
    fn the_hud_shows_a_for_auto_and_the_select_row_starts_with_the_format() {
        let (mut ui, _l, b) = new_ui();
        let names = HudNames::of(Seat::P1, Name::new("own"), Name::new("peer"));
        let h = HudFacts {
            latency: Some(2),
            rtt_ms: Some(33),
            recommend: Some(2),
            names: Some(names),
            ..HudFacts::default()
        };
        ui.hud(true, h);
        assert_eq!(row_text(b, ROW_HUD), "Delay 2F (2F)  ping 33ms");
        ui.hud(true, HudFacts { auto: true, ..h });
        assert_eq!(row_text(b, ROW_HUD), "Delay 2F (A)  ping 33ms");
        ui.hud(
            true,
            HudFacts {
                latency: Some(3),
                ..h
            },
        );
        assert_eq!(row_text(b, ROW_HUD), "Delay 3F (2F)  ping 33ms");
        ui.hud(
            true,
            HudFacts {
                auto: true,
                format: Some(Mode::Rollback),
                ..h
            },
        );
        assert_eq!(row_text(b, ROW_HUD), "rollback・Delay 2F (A)  ping 33ms");
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), SHOW_HUD | SHOW_NAMES);
        assert_eq!(row_text(b, ROW_NAME_L), "1P own");
        assert_eq!(row_text(b, ROW_NAME_R), "peer 2P");
        ui.hud(
            true,
            HudFacts {
                format: Some(Mode::Delay),
                ..h
            },
        );
        assert_eq!(row_text(b, ROW_HUD), "delay・Delay 2F (2F)  ping 33ms");
        ui.hud(true, h);
        assert_eq!(row_text(b, ROW_HUD), "Delay 2F (2F)  ping 33ms");
        ui.hud(false, HudFacts::default());
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), 0);
    }

    #[test]
    fn select_screen_and_settings_rows_render_to_preview_images() {
        use crate::overlay_win::{FONT_FACES, FONT_HEIGHT, FONT_SCALE, gdi_atlas};
        const W: u32 = 640;
        const H: u32 = 480;
        let atlas = gdi_atlas(&FONT_FACES, FONT_HEIGHT, FONT_SCALE).expect("GDI の文字の表");
        let gradient: Vec<u8> = (0..W * H)
            .flat_map(|i| {
                let (x, y) = (i % W, i / W);
                [(40 + x * 180 / W) as u8, (60 + y * 120 / H) as u8, 110]
            })
            .collect();
        let load = |var: &str| -> Option<Vec<u8>> {
            let path = std::env::var_os(var)?;
            let mut r = png::Decoder::new(std::io::Cursor::new(std::fs::read(path).ok()?))
                .read_info()
                .ok()?;
            let mut buf = vec![0u8; r.output_buffer_size()?];
            let info = r.next_frame(&mut buf).ok()?;
            (info.width == W
                && info.height == H
                && info.color_type == png::ColorType::Rgb
                && info.bit_depth == png::BitDepth::Eight)
                .then_some(buf)
        };
        let bg_select = load("HIGANR_PREVIEW_BG_SELECT").unwrap_or_else(|| gradient.clone());
        let bg_menu = load("HIGANR_PREVIEW_BG_MENU").unwrap_or_else(|| gradient.clone());
        let out_dir = std::env::var_os("HIGANR_PREVIEW_DIR").map(std::path::PathBuf::from);
        let stamp = format!(
            "{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        let shot = |b: &Board, bg: &[u8], kind: &str| {
            let mut sheet = bg.to_vec();
            let mut quads = 0usize;
            draw_board(b, &atlas.spec, W, H, &mut |v| {
                quads += blit_quads(
                    v,
                    &atlas.texels,
                    atlas.spec.tex_w,
                    atlas.spec.tex_h,
                    &mut sheet,
                    W,
                    H,
                    0,
                    0,
                );
            });
            assert!(quads > 0 && sheet != bg, "{kind}: 何も描かれていない");
            if let Some(dir) = &out_dir {
                std::fs::create_dir_all(dir).expect("プレビューのフォルダ");
                let path = dir.join(format!("u4_{kind}_{stamp}.png"));
                assert!(!path.exists(), "既存のファイルを上書きしない");
                let big = higanr_core::overlay::upscale_rgb(&sheet, W, H, 2);
                let png = higanr_core::shot::encode_png(W * 2, H * 2, &big).expect("PNG に符号化");
                std::fs::write(&path, png).expect("PNG を書く");
                println!("PNG: {}", path.display());
            }
        };
        let names = HudNames::of(Seat::P1, Name::new("Reimu-fan"), Name::new("Marisa-fan"));
        let facts_with = |latency, recommend, auto, format| HudFacts {
            latency: Some(latency),
            rtt_ms: Some(33),
            recommend,
            auto,
            format,
            names: Some(names),
            ..HudFacts::default()
        };
        // キャラセレクト（画面 8）: ホスト・auto。名前の帯（上の左右）・左下の「rollback・Delay 2F (A)  ping 33ms」。左上の接続の行は出さない。
        let (mut ui, _l, b) = new_ui();
        let c = UiFacts {
            conn: Conn::Connected {
                seat: Seat::P1,
                host: true,
                shape: Some((Mode::Rollback, 2)),
                watch_off: false,
            },
            net: NetPhase::Open,
            ..facts()
        };
        let mut g = Fake::on_screen(8, 0);
        g.step(&mut ui, &c, 0);
        ui.hud(true, facts_with(2, Some(2), true, Some(Mode::Rollback)));
        assert_eq!(row_text(b, ROW_HUD), "rollback・Delay 2F (A)  ping 33ms");
        assert_eq!(row_text(b, ROW_NAME_L), "1P Reimu-fan");
        assert_eq!(row_text(b, ROW_NAME_R), "Marisa-fan 2P");
        assert_eq!(
            b.show() & (SHOW_TL0 | SHOW_TL1),
            0,
            "キャラセレクトに左上の接続の行が出た（画面 8 から先は出さない）"
        );
        shot(b, &bg_select, "select_host_auto");
        let (mut ui, _l, b) = new_ui();
        let mut g = Fake::on_screen(8, 0);
        g.step(&mut ui, &c, 0);
        ui.hud(true, facts_with(3, Some(2), false, Some(Mode::Delay)));
        assert_eq!(row_text(b, ROW_HUD), "delay・Delay 3F (2F)  ping 33ms");
        shot(b, &bg_select, "select_guest_manual");
        let (mut ui, _l, b) = new_ui();
        ui.hud(
            true,
            HudFacts {
                rewound: Some(1),
                ..facts_with(2, Some(2), true, None)
            },
        );
        assert_eq!(row_text(b, ROW_HUD), "Delay 2F (A)  ping 33ms  rollback 1");
        shot(b, &bg_select, "match_auto");
        let (mut ui, _l, b) = new_ui();
        let mut g = Fake::on_match_screen(4);
        let f = facts();
        g.step(&mut ui, &f, Z);
        g.step(&mut ui, &f, 0);
        open_settings(&mut g, &mut ui, b);
        let rows = menu_rows(b);
        assert!(
            rows.iter().any(|l| l == "> 同期方式\u{3000}◀ rollback ▶"),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|l| l == "  ディレイ\u{3000}◀ 1F ▶"),
            "{rows:?}"
        );
        shot(b, &bg_menu, "menu_cursor_on_format");
    }

    #[test]
    fn update_notice_rows_render_to_preview_images() {
        use crate::overlay_win::{FONT_FACES, FONT_HEIGHT, FONT_SCALE, gdi_atlas};
        use higanr_core::overlay::Vertex;
        const W: u32 = 640;
        const H: u32 = 480;
        const COLORS: [(&str, u32); 3] = [
            ("green", 0xFF80_E0A0),
            ("red", 0xFFFF_5050),
            ("pink", 0xFFFF_9090),
        ];
        assert_eq!(
            COLORS[2].1,
            lay::COLOR_NOTICE,
            "既定の色の見本が定数とずれた"
        );
        let atlas = gdi_atlas(&FONT_FACES, FONT_HEIGHT, FONT_SCALE).expect("GDI の文字の表");
        let gradient: Vec<u8> = (0..W * H)
            .flat_map(|i| {
                let (x, y) = (i % W, i / W);
                [(40 + x * 180 / W) as u8, (60 + y * 120 / H) as u8, 110]
            })
            .collect();
        let load = |var: &str| -> Option<Vec<u8>> {
            let path = std::env::var_os(var)?;
            let mut r = png::Decoder::new(std::io::Cursor::new(std::fs::read(path).ok()?))
                .read_info()
                .ok()?;
            let mut buf = vec![0u8; r.output_buffer_size()?];
            let info = r.next_frame(&mut buf).ok()?;
            (info.width == W
                && info.height == H
                && info.color_type == png::ColorType::Rgb
                && info.bit_depth == png::BitDepth::Eight)
                .then_some(buf)
        };
        let bg_menu = load("HIGANR_PREVIEW_BG_MENU").unwrap_or_else(|| gradient.clone());
        let bg_end = load("HIGANR_PREVIEW_BG_END").unwrap_or_else(|| gradient.clone());
        let out_dir = std::env::var_os("HIGANR_PREVIEW_DIR").map(std::path::PathBuf::from);
        let stamp = {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let (days, rem) = (secs / 86_400, secs % 86_400);
            // 1970-01-01からの日数を年月日へ（グレゴリオ暦。UTC）。
            let z = days as i64 + 719_468;
            let era = z.div_euclid(146_097);
            let doe = z.rem_euclid(146_097);
            let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let day = doy - (153 * mp + 2) / 5 + 1;
            let month = if mp < 10 { mp + 3 } else { mp - 9 };
            let year = yoe + era * 400 + i64::from(month <= 2);
            format!(
                "{year:04}{month:02}{day:02}_{:02}{:02}{:02}_{}",
                rem / 3600,
                rem % 3600 / 60,
                rem % 60,
                std::process::id()
            )
        };
        let write = |name: &str, w: u32, h: u32, rgb: &[u8]| {
            if let Some(dir) = &out_dir {
                std::fs::create_dir_all(dir).expect("プレビューのフォルダ");
                let path = dir.join(format!("update_notice_{name}_{stamp}.png"));
                assert!(!path.exists(), "既存のファイルを上書きしない");
                let big = higanr_core::overlay::upscale_rgb(rgb, w, h, 2);
                let png = higanr_core::shot::encode_png(w * 2, h * 2, &big).expect("PNG に符号化");
                std::fs::write(&path, png).expect("PNG を書く");
                println!("PNG: {}", path.display());
            }
        };
        // 色だけ差し替えて描く。返すのは（絵・差し替えた頂点の数・知らせの文字のyの範囲）。
        let render = |b: &Board, bg: &[u8], color: u32| {
            let mut sheet = bg.to_vec();
            let (mut replaced, mut y0, mut y1) = (0usize, f32::MAX, f32::MIN);
            draw_board(b, &atlas.spec, W, H, &mut |v| {
                let mut v: Vec<Vertex> = v.to_vec();
                for p in &mut v {
                    if p.color == lay::COLOR_NOTICE {
                        p.color = color;
                        replaced += 1;
                        y0 = y0.min(p.y);
                        y1 = y1.max(p.y);
                    }
                }
                blit_quads(
                    &v,
                    &atlas.texels,
                    atlas.spec.tex_w,
                    atlas.spec.tex_h,
                    &mut sheet,
                    W,
                    H,
                    0,
                    0,
                );
            });
            assert!(replaced > 0 && sheet != bg, "知らせの頂点が描かれていない");
            (sheet, y0, y1)
        };
        let scene = |kind: &str, b: &Board, bg: &[u8]| {
            let mut strips: Vec<u8> = Vec::new();
            let mut strip_h = 0;
            for (name, color) in COLORS {
                let (sheet, y0, y1) = render(b, bg, color);
                write(&format!("{kind}_{name}"), W, H, &sheet);
                // 比べる絵は知らせの上下に30画素ずつ足した帯を3色ぶん縦に並べる。
                let top = (y0 as i64 - 30).clamp(0, H as i64 - 1) as u32;
                let bottom = ((y1 as i64 + 30).clamp(1, H as i64) as u32).max(top + 1);
                strip_h = bottom - top;
                strips.extend_from_slice(&sheet[(top * W * 3) as usize..(bottom * W * 3) as usize]);
            }
            write(&format!("{kind}_compare"), W, strip_h * 3, &strips);
        };
        // 場面 (a): ネット対戦メニューの一番下。
        let (mut ui, b) = update_ui(BUILD);
        news_is_out(Found::Newer(higanr_core::update_notice::Version::new(
            1, 2, 2,
        )));
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), 0);
        open_the_menu(&mut g, &mut ui, &facts());
        g.step(&mut ui, &facts(), 0);
        let rows = menu_rows(b);
        let n = rows.len();
        assert_eq!(
            rows[n - 2],
            "新しいバージョンがリリースされています（v1.2.2）"
        );
        assert_eq!(row_style(b, n - 1), RowStyle::Notice);
        scene("menu", b, &bg_menu);
        // 場面 (b): 接続中・マッチ終了時のメニュー・名前の帯あり。
        let (mut ui, b) = update_ui(BUILD);
        let mut g = Fake::on_screen(8, 0);
        g.scenes = (SCENE_MATCH, 0);
        g.end_menu = true;
        g.step(&mut ui, &linked(true), 0);
        let names = HudNames::of(Seat::P1, Name::new("Reimu-fan"), Name::new("Marisa-fan"));
        ui.hud(
            true,
            HudFacts {
                latency: Some(2),
                rtt_ms: Some(33),
                recommend: Some(2),
                names: Some(names),
                ..HudFacts::default()
            },
        );
        assert_eq!(b.show() & SHOW_UPDATE, SHOW_UPDATE);
        assert_eq!(b.show() & SHOW_NAMES, SHOW_NAMES);
        assert_eq!(
            row_text(b, ROW_UPD0),
            "新しいバージョンがリリースされています（v1.2.2）"
        );
        scene("end", b, &bg_end);
    }

    #[test]
    fn the_connected_line_stops_at_the_difficulty_screen_and_other_lines_stay() {
        const CONNECTED: &str = "接続しました。あなたは1Pです。（rollback・delay 1）";
        let c = connected_facts();
        let shown = |b: &Board| b.show() & (SHOW_TL0 | SHOW_TL1) != 0;
        for screen in [6, 7] {
            let (mut ui, _l, b) = new_ui();
            let mut g = Fake::on_screen(screen, 0);
            g.step(&mut ui, &c, 0);
            assert_eq!(row_text(b, ROW_TL0), CONNECTED, "画面 {screen} で出ない");
        }
        let (mut ui, _l, b) = new_ui();
        let mut g7 = Fake::on_screen(7, 0);
        g7.step(&mut ui, &c, 0);
        assert_eq!(row_text(b, ROW_TL0), CONNECTED, "画面 7 で出ない");
        let mut g8 = Fake::on_screen(8, 0);
        g8.step(&mut ui, &c, 0);
        assert!(!shown(b), "画面 7 から 8 へ移っても左上の行が残った");
        let (mut ui, _l, b) = new_ui();
        let mut g16 = Fake::on_screen(16, 0);
        g16.step(&mut ui, &c, 0);
        assert!(!shown(b), "画面 16 で左上の行が出た");
        let (mut ui, _l, b) = new_ui();
        let mut g7 = Fake::on_screen(7, 0);
        g7.step(&mut ui, &c, 0);
        assert!(shown(b), "画面 7 で出ていない（前提）");
        let mut m = Fake::on_match_screen(0);
        m.scenes = (2, 0);
        m.step(&mut ui, &c, 0);
        assert!(!shown(b), "試合の場面に左上の行が残った");
        let mut g8 = Fake::on_screen(8, 0);
        g8.step(&mut ui, &c, 0);
        assert!(!shown(b), "試合から戻った画面 8 で左上の行が出た");
        let (mut ui, _l, b) = new_ui();
        let mut g8 = Fake::on_screen(8, 0);
        g8.step(&mut ui, &found_facts(false), 0);
        assert!(
            row_text(b, ROW_TL0).starts_with("相手が見つかりました"),
            "{}",
            row_text(b, ROW_TL0)
        );
        let (mut ui, link, b) = new_ui();
        link.publish(&Note {
            seq: 1,
            event: Event::RoomWaiting,
            state: CodeState::SpectateWaiting,
            room: higanr_core::net_menu::sanitize_room("Synth", |_| true),
            since: 1_000_000 - 61,
            ..Note::default()
        });
        let mut g8 = Fake::on_screen(8, 0);
        g8.step(&mut ui, &facts(), 0);
        assert_ne!(b.show() & SHOW_WAIT, 0, "観戦者の待ちの行が画面 8 で消えた");
    }

    fn spec() -> AtlasSpec {
        AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap()
    }

    #[test]
    fn draw_board_stacks_rows_by_their_own_heights_so_gaps_are_shorter_than_a_row() {
        let s = spec();
        let (sw, sh) = (640u32, 480u32);
        let row_h = lay::row_height(&s);
        let rows = [
            ("対戦", RowStyle::Title),
            ("", RowStyle::GapSmall),
            ("> a", RowStyle::Selected),
            ("", RowStyle::GapMedium),
            ("b", RowStyle::Normal),
            ("", RowStyle::GapLarge),
            ("c", RowStyle::Normal),
        ];
        let b = board();
        for (i, (text, style)) in rows.iter().enumerate() {
            b.set_row(i, text, *style);
        }
        b.set_menu_len(rows.len());
        b.set_show(SHOW_MENU);
        let mut batches: Vec<Vec<Vertex>> = Vec::new();
        draw_board(b, &s, sw, sh, &mut |v| batches.push(v.to_vec()));
        let styles: Vec<RowStyle> = rows.iter().map(|r| r.1).collect();
        let total = lay::rows_height(&s, &styles);
        assert_eq!(total, 4 * row_h + row_h / 3 + row_h / 2 + row_h);
        let bx = lay::menu_box(sw, sh, total, 0);
        let border: Vec<f32> = batches[0][6..12].iter().map(|v| v.y).collect();
        let span = border.iter().cloned().fold(f32::MIN, f32::max)
            - border.iter().cloned().fold(f32::MAX, f32::min);
        assert_eq!(span, bx.h as f32, "枠の高さが行の高さの合計");
        // 1まとまり目は枠の下地。文字の行は上から順に1行1まとまり: 見出し・a・b・c。
        let top = |k: usize| batches[1 + k].iter().map(|v| v.y).fold(f32::MAX, f32::min);
        let at = |k: usize| top(k) - top(0);
        assert_eq!(at(1), (row_h + row_h / 3) as f32, "小さな隙間の分");
        assert_eq!(
            at(2),
            (2 * row_h + row_h / 3 + row_h / 2) as f32,
            "中くらいの隙間の分"
        );
        assert_eq!(
            at(3),
            (3 * row_h + row_h / 3 + row_h / 2 + row_h) as f32,
            "大きな隙間は 1 行分"
        );
    }

    #[test]
    fn draw_board_puts_the_menu_in_the_middle_and_the_lines_at_the_corners() {
        let s = spec();
        let (sw, sh) = (640u32, 480u32);
        let texels = vec![0xFFFF_FFFFu32; s.texel_count()];
        let b = board();
        let mut n = 0;
        assert_eq!(draw_board(b, &s, sw, sh, &mut |_| n += 1), 0);
        assert_eq!(n, 0);
        b.set_row(0, "接続コード", RowStyle::Title);
        b.set_row(1, "", RowStyle::Normal);
        b.set_row(2, "> 戻る", RowStyle::Selected);
        b.set_menu_len(3);
        b.set_show(SHOW_MENU);
        let mut canvas = vec![200u8; (sw * sh * 3) as usize];
        let mut batches = Vec::new();
        let got = draw_board(b, &s, sw, sh, &mut |v| {
            batches.push(v.to_vec());
            blit_quads(v, &texels, s.tex_w, s.tex_h, &mut canvas, sw, sh, 0, 0);
        });
        assert_eq!(got, 3);
        let px = |x: u32, y: u32| {
            let i = ((y * sw + x) * 3) as usize;
            [canvas[i], canvas[i + 1], canvas[i + 2]]
        };
        assert!(px(2, 2).iter().all(|&c| c < 100), "枠の外は暗い");
        let row_h = lay::row_height(&s);
        let w = lay::text_width(&s, "> 戻る").max(lay::text_width(&s, "接続コード"));
        let bx = lay::menu_box(sw, sh, 3 * row_h, w);
        assert_eq!(bx.x, (sw - bx.w) / 2);
        assert_eq!(px(bx.x, bx.y + bx.h / 2), [0xB0, 0xC0, 0xE0], "枠線");
        assert!(px(bx.x - 1, bx.y + bx.h / 2)[0] < 100);
        let _ = row_h;
        b.set_show(SHOW_TL0 | SHOW_HUD);
        b.set_row(ROW_TL0, "abc", RowStyle::Normal);
        b.set_row(ROW_HUD, "xy", RowStyle::Normal);
        let mut canvas = vec![0u8; (sw * sh * 3) as usize];
        let got = draw_board(b, &s, sw, sh, &mut |v| {
            blit_quads(v, &texels, s.tex_w, s.tex_h, &mut canvas, sw, sh, 0, 0);
        });
        assert_eq!(got, 2);
        let px = |x: u32, y: u32| {
            let i = ((y * sw + x) * 3) as usize;
            [canvas[i], canvas[i + 1], canvas[i + 2]]
        };
        assert_eq!(px(TL_X, TL_Y), [255, 255, 255]);
        assert_eq!(px(TL_X, TL_Y - lay::PAD - 1), [0, 0, 0]);
        // マッチ中の行（通信の状態）は下の枠の左に出る。帯は y = 465..480（画面の下端から`HUD_BAND_H`）。文字は x = `HUD_EDGE_X`・y = 466 から。
        // 下地は半透明の黒なので黒のまま。文字の白で見る。上の枠には何も描かない。
        let top = lay::hud_bottom_y(sh);
        assert_eq!(top, 465);
        assert_eq!(px(lay::HUD_EDGE_X, top + HUD_Y), [255, 255, 255]);
        assert_eq!(
            px(lay::HUD_EDGE_X, top + HUD_Y + 13),
            [255, 255, 255],
            "文字の枠は高さ 14（画面の最後の行 479 まで）"
        );
        assert_eq!(
            px(lay::HUD_EDGE_X, top - 1),
            [0, 0, 0],
            "帯の上には何も描かない"
        );
        assert_eq!(
            px(HUD_X, HUD_Y),
            [0, 0, 0],
            "上の枠の左（今までの試合中の行の置き場）には何も描かない"
        );
    }

    #[test]
    fn a_too_long_band_is_cut_before_the_timer_and_the_fps_text() {
        // coreの幅の検査は実物の文字の表と同じ枠（高さ14 → 半角7・全角14）で数えている。
        // 実物の文字の高さを変えたらcoreの検査の表も合わせる（ここで落ちる）。
        const { assert!(crate::overlay_win::FONT_HEIGHT == 14) };
        let s = spec();
        let long = "Delay* 9 (9)  ping 4294967295ms  rollback 255  ".repeat(3);
        for (row, bit, right_limit, max_w) in [
            (
                ROW_HUD,
                SHOW_HUD,
                lay::HUD_BOTTOM_RIGHT,
                lay::HUD_BOTTOM_TEXT_MAX_W,
            ),
            (ROW_WAIT, SHOW_WAIT, lay::HUD_RIGHT, HUD_TEXT_MAX_W),
            (ROW_NAME_L, SHOW_NAMES, lay::HUD_RIGHT, lay::HUD_NAME_MAX_W),
        ] {
            let b = board();
            b.set_row(row, &long, RowStyle::Normal);
            b.set_show(bit);
            let mut right = 0f32;
            let mut quads = 0usize;
            let got = draw_board(b, &s, 640, 480, &mut |v| {
                quads += v.len() / 6;
                for p in v {
                    right = right.max(p.x);
                }
            });
            assert_eq!(got, 1, "行 {row}");
            assert!(
                right <= right_limit as f32,
                "行 {row}: 右端 {right} が {right_limit}"
            );
            let fit: String = {
                let mut w = 0;
                long.chars()
                    .take_while(|&c| {
                        w += lay::text_width_plain(&s, &c.to_string());
                        w <= max_w
                    })
                    .collect()
            };
            let drawn = fit.chars().filter(|&c| c != ' ').count();
            assert!(drawn > 20, "{fit}");
            assert_eq!(quads, 1 + drawn, "行 {row}: {fit}");
        }
        let b = board();
        b.set_row(ROW_NAME_R, &long, RowStyle::Normal);
        b.set_show(SHOW_NAMES);
        let (mut left, mut right) = (f32::MAX, 0f32);
        let got = draw_board(b, &s, 640, 480, &mut |v| {
            for p in v {
                left = left.min(p.x);
                right = right.max(p.x);
            }
        });
        assert_eq!(got, 1);
        assert!(left >= (640 - lay::HUD_RIGHT) as f32 - 0.5, "左端 {left}");
        assert!(right <= 640.0, "右端 {right}");
    }

    /// 実物の関数が描く表と同じ、ゲームの文字（英数字`!`〜`~`。`` ` `` を除く）をコピーした文字の表（8 × 12 の塗りつぶしの文字）。
    fn game_font_spec() -> AtlasSpec {
        use higanr_core::overlay::{GAME_ADVANCE, GameGlyphSrc, apply_game_font, texels};
        let glyphs = ui_glyphs();
        let base = AtlasSpec::fit_game(7, 14, &glyphs).unwrap();
        let t = texels(&base, &vec![0u8; base.texel_count()]).unwrap();
        let px = vec![0xFFFF_FFFFu32; 8 * 12];
        let srcs: Vec<GameGlyphSrc> = (0x21u8..=0x7E)
            .filter(|&c| c != b'`')
            .map(|c| GameGlyphSrc {
                c: c as char,
                w: 8,
                h: 12,
                pixels: &px,
                quad_w: 8,
                quad_h: 12,
            })
            .collect();
        apply_game_font(&base, &t, &srcs, GAME_ADVANCE).unwrap().0
    }

    #[test]
    fn the_waiting_line_is_drawn_like_the_match_line_and_never_over_it() {
        use higanr_core::overlay::GAME_ADVANCE;
        let s = game_font_spec();
        let text = "abcd";
        let b = board();
        b.set_row(ROW_WAIT, text, RowStyle::Normal);
        b.set_row(ROW_HUD, text, RowStyle::Normal);
        b.set_row(ROW_TL0, text, RowStyle::Normal);
        let batch = |show: u32| -> Vec<Vec<Vertex>> {
            b.set_show(show);
            let mut batches: Vec<Vec<Vertex>> = Vec::new();
            draw_board(b, &s, 640, 480, &mut |v| batches.push(v.to_vec()));
            batches
        };
        let hud = batch(SHOW_HUD);
        let wait = batch(SHOW_WAIT);
        assert_eq!((hud.len(), wait.len()), (1, 1));
        // 同じ形（下地・文字の送り幅まで。置き場だけが違う）。待っている行は上の枠の左（x = 6・y = 0）。マッチ中の行は下の枠の左（x = `HUD_EDGE_X`・y = 465）。
        // 頂点ごとに座標の差が全部同じで、色と文字の位置（u・v）は同じ。
        let (dx, dy) = (hud[0][0].x - wait[0][0].x, hud[0][0].y - wait[0][0].y);
        assert_eq!(
            (dx, dy),
            (
                lay::HUD_EDGE_X as f32 - HUD_X as f32,
                lay::hud_bottom_y(480) as f32
            )
        );
        assert_eq!(hud[0].len(), wait[0].len());
        for (h, w) in hud[0].iter().zip(&wait[0]) {
            assert_eq!(
                (h.x - w.x, h.y - w.y, h.color, h.u, h.v),
                (dx, dy, w.color, w.u, w.v),
                "待っている行が試合中の行と同じ描き方でない"
            );
        }
        let lefts =
            |v: &[Vertex]| -> Vec<f32> { (1..v.len() / 6).map(|q| v[q * 6].x + 0.5).collect() };
        let step: Vec<f32> = lefts(&wait[0]).windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(
            step, [7.0; 3],
            "ゲームの文字（{GAME_ADVANCE} 画素送り）で描いた"
        );
        // 両方の旗が立っていればマッチ中の行だけ（待っている行は名前の帯と同じ上の枠の左に出るので重なる）。
        assert_eq!(batch(SHOW_HUD | SHOW_WAIT), hud);
        assert_eq!(
            batch(SHOW_NAMES | SHOW_WAIT),
            Vec::<Vec<Vertex>>::new(),
            "名前の帯の行が空なら何も描かず、待っている行も出さない"
        );
        assert_eq!(batch(SHOW_TL0).len(), 1, "左上の行は別の置き場");
        assert_ne!(batch(SHOW_TL0), hud);
        b.set_row(ROW_WAIT, &"abcdefghij".repeat(10), RowStyle::Normal);
        let mut right = 0f32;
        b.set_show(SHOW_WAIT);
        draw_board(b, &s, 640, 480, &mut |v| {
            for p in v {
                right = right.max(p.x);
            }
        });
        assert!(right <= lay::HUD_RIGHT as f32, "右端 {right}");
    }

    #[test]
    fn the_match_row_alone_is_drawn_in_gdi_glyphs_on_a_game_font_table() {
        use higanr_core::overlay::GAME_ADVANCE;
        let s = game_font_spec();
        let b = board();
        b.set_row(ROW_TL0, "abcd", RowStyle::Normal);
        b.set_row(ROW_HUD, "abcd", RowStyle::Normal);
        b.set_show(SHOW_TL0 | SHOW_HUD);
        let mut batches: Vec<Vec<Vertex>> = Vec::new();
        let got = draw_board(b, &s, 640, 480, &mut |v| batches.push(v.to_vec()));
        assert_eq!(got, 2);
        let lefts =
            |v: &[Vertex]| -> Vec<f32> { (1..v.len() / 6).map(|q| v[q * 6].x + 0.5).collect() };
        let back_w = |v: &[Vertex]| v[1].x - v[0].x;
        let (tl, hud) = (&batches[0], &batches[1]);
        let step = |xs: Vec<f32>| -> Vec<f32> { xs.windows(2).map(|w| w[1] - w[0]).collect() };
        assert_eq!(step(lefts(hud)), [7.0; 3], "試合中の行は 7 画素送り");
        assert_eq!(back_w(hud), (4 * 7 + 2 * lay::PAD) as f32, "下地は文字の幅");
        assert_eq!(
            step(lefts(tl)),
            [GAME_ADVANCE as f32; 3],
            "左上の行はゲームの文字のまま"
        );
    }

    #[test]
    fn leaving_the_menu_scene_closes_the_menu() {
        let (mut ui, _l, b) = new_ui();
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        assert!(ui.menu_active());
        g.scenes = (2, 0);
        assert_eq!(g.step(&mut ui, &facts(), 0), UiOut::default());
        assert!(!ui.menu_active());
        assert_eq!(b.show() & SHOW_MENU, 0);
        let mut g = Fake::on_match_screen(4);
        let (mut ui, _l, _b) = new_ui();
        g.step(&mut ui, &facts(), Z);
        assert!(ui.menu_active());
        g.view.as_mut().unwrap().screen = 1;
        assert_eq!(g.step(&mut ui, &facts(), 0), UiOut::default());
        assert!(!ui.menu_active());
    }

    fn code_play_config() -> crate::netplay::Config {
        crate::netplay::Config {
            check: crate::netplay::CheckSource::InputDigest,
            require_net_selection: true,
            ..crate::netplay::Config::default()
        }
    }

    fn pair_link_cfg(peer_port: u16, nonce: u64) -> higanr_core::link::Config {
        higanr_core::link::Config::single(
            higanr_core::handshake::Config {
                session_id: 9,
                role_nonce: nonce,
                latency: 2,
                name: higanr_core::handshake::Name::new("t"),
                host_seat: Seat::P1,
            },
            higanr_core::auth::Secret([5; 16]),
            std::net::SocketAddr::from(([127, 0, 0, 1], peer_port)),
            higanr_core::seal::KeySeed([0x4B; 32]),
        )
    }

    #[test]
    fn pasting_while_the_menu_holds_screen_six_connects_and_closes_the_menu() {
        use crate::netplay::fake::{FRAME_US, FakeNet};
        use crate::netplay::{LinkSlot, Phase};
        let (a, b) = (
            std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            std::net::SocketAddr::from(([127, 0, 0, 1], 2)),
        );
        let (mut na, mut nb, clock) = FakeNet::pair(a, b, 1_000);
        let (slot_a, slot_b) = (Arc::new(LinkSlot::new()), Arc::new(LinkSlot::new()));
        let cfg = code_play_config();
        let mut np_a = NetPlay::with_code_slot(Arc::clone(&slot_a), cfg, &mut na);
        let mut np_b = NetPlay::with_code_slot(Arc::clone(&slot_b), cfg, &mut nb);
        let (mut ui, _link, _board) = new_ui();
        let mut g = Fake::on_match_screen(4);
        let mut at = clock.get();
        let mut tick = |g: &mut Fake,
                        ui: &mut NetUi,
                        np_a: &mut NetPlay,
                        np_b: &mut NetPlay,
                        w: u16|
         -> (Action, UiOut) {
            clock.set(clock.get().max(at));
            let r = g.step_net(ui, np_a, &mut na, w);
            clock.set(clock.get().max(at + FRAME_US / 2));
            np_b.tend(&mut nb);
            at += FRAME_US;
            r
        };
        let (act, _) = tick(&mut g, &mut ui, &mut np_a, &mut np_b, Z);
        assert_eq!(act, Action::Menu);
        assert!(ui.menu_active());
        assert_eq!(g.feeds, vec![[0, 0, 0]]);
        slot_a.offer(pair_link_cfg(2, 0xF000));
        slot_b.offer(pair_link_cfg(1, 1));
        let mut connected_at = None;
        for t in 0..250u32 {
            let (act, _) = tick(&mut g, &mut ui, &mut np_a, &mut np_b, 0);
            assert_eq!(
                act,
                Action::Menu,
                "tick {t}: 画面 6 でメニューが握っていない tick がある"
            );
            if connected_at.is_none() && matches!(np_a.ui_conn(0).0, Conn::Connected { .. }) {
                connected_at = Some(t);
            }
            if connected_at.is_some_and(|c| t >= c + 6) {
                break;
            }
        }
        let c = connected_at.expect("貼った後、画面 6 のメニューを開いたままでつながらない");
        assert_eq!(np_a.phase(), Phase::Selecting, "手入れが繋がりを見ていない");
        assert!(slot_a.is_connected(), "見張りへ繋がったと知らせていない");
        assert!(
            !ui.menu_active(),
            "つながった後（{c} tick 目）もメニューが閉じない"
        );
        assert!(g.feeds.len() > 6);
        // 画面7（難易度。ネットの選択の画面）でキャンセルすると確認の枠が出る。枠の間は毎tickメニューが入力を占有する。
        // このとき場面ごとの判断（`dispatch`の`menu_tick`）まで呼ぶと、同じtickにfeedが2回になる（偽のゲームが落とす）。
        g.view = Some(MenuView {
            screen: 7,
            sub: 1,
            trans: 0,
            cursor: 0,
            ..MenuView::default()
        });
        let (act, _) = tick(&mut g, &mut ui, &mut np_a, &mut np_b, X);
        assert_eq!(act, Action::Menu, "画面 7 の取消を握っていない");
        assert!(ui.menu_active(), "確かめの枠が開かない");
        assert_eq!(g.last_feed(), Some([0, 0, 0]));
        for _ in 0..5 {
            let (act, _) = tick(&mut g, &mut ui, &mut np_a, &mut np_b, 0);
            assert_eq!(act, Action::Menu, "確かめの枠の間は握り続ける");
            assert_eq!(g.last_feed(), Some([0, 0, 0]));
        }
        assert!(ui.menu_active());
    }

    /// 自分と相手のネットワーク種別のフラグで、左上の警告（最大5行）がどの文になるかをセッションを動かして検証する。
    /// 表の列: 自分の「宛先ごとに変わる型か」・自分のUPnP・相手のコードのフラグ（Noneは読んでいない）・出る行。
    #[test]
    fn a_timed_out_handshake_turns_the_corner_into_warn_rows_chosen_by_the_nat_marks_until_it_connects()
     {
        use crate::netplay::LinkSlot;
        use crate::netplay::fake::FakeNet;
        use higanr_core::nat::{NatMark, Symmetry};
        const F1: &[&str] = &[
            "接続失敗：",
            "不明な原因で相手からのUDPパケットが届きません。",
            "iniをdetail=1にして再度試し、zipにしたlogsフォルダを添えて開発者に伝えてください。",
            "（README「接続できないとき」1）",
        ];
        const F2: &[&str] = &[
            "接続失敗：",
            "相手がシンメトリックNATであり、こちらはUPnPが使用不可です。",
            "ルーターのUPnPをオンにするか、ポート開放してください。",
            "（README「接続できないとき」2）",
        ];
        const F3: &[&str] = &[
            "接続失敗：",
            "こちらがシンメトリックNATです。",
            "相手側のポート開放が必要です。",
            "相手もシンメトリックNATなら通信できません。",
            "（README「接続できないとき」3）",
        ];
        const F4: &[&str] = &[
            "接続不可：",
            "こちらも相手もシンメトリックNATのため、直接接続できません。",
            "（README「接続できないとき」4）",
        ];
        let mark = |symmetry, upnp| Some(NatMark { symmetry, upnp });
        let read = |b: &Board, i: usize| {
            let mut buf = [0u8; MAX_TEXT_UTF8];
            b.read_row(i, &mut buf).map(|(t, s)| (t.to_string(), s))
        };
        let rows = [
            (SHOW_TL0, ROW_TL0),
            (SHOW_TL1, ROW_TL1),
            (SHOW_TL2, ROW_TL2),
            (SHOW_TL3, ROW_TL3),
            (SHOW_TL4, ROW_TL4),
        ];
        let all_rows = SHOW_TL0 | SHOW_TL1 | SHOW_TL2 | SHOW_TL3 | SHOW_TL4;
        type Case = (Option<bool>, bool, Option<NatMark>, &'static [&'static str]);
        let cases: [Case; 8] = [
            (None, false, None, F1),
            (Some(false), false, mark(Symmetry::No, false), F1),
            (Some(true), false, None, F3),
            (Some(true), false, mark(Symmetry::No, false), F3),
            (Some(false), false, mark(Symmetry::Yes, false), F2),
            (Some(false), true, mark(Symmetry::Yes, false), F1),
            (Some(true), true, mark(Symmetry::Yes, false), F1),
            (Some(true), false, mark(Symmetry::Yes, false), F4),
        ];
        for (varies, upnp, peer, want) in cases {
            let tag = format!("自分 varies={varies:?} upnp={upnp} / 相手 {peer:?}");
            let early = want == F4;
            let (a, b) = (
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
                std::net::SocketAddr::from(([127, 0, 0, 1], 2)),
            );
            let (mut na, mut nb, clock) = FakeNet::pair(a, b, 1_000);
            na.varies = varies;
            na.upnp = upnp;
            let (slot_a, slot_b) = (Arc::new(LinkSlot::new()), Arc::new(LinkSlot::new()));
            let cfg = code_play_config();
            let mut np_a = NetPlay::with_code_slot(Arc::clone(&slot_a), cfg, &mut na);
            let mut np_b = NetPlay::with_code_slot(Arc::clone(&slot_b), cfg, &mut nb);
            let (mut ui, link, board) = new_ui();
            let (head, tail) = Note::edges_of("AbCdEfGh");
            link.publish(&Note {
                seq: 1,
                event: higanr_core::net_menu::Event::Made,
                state: higanr_core::net_menu::CodeState::Linking,
                code_len: 8,
                head,
                tail,
                since: 1_000_000 - 42,
                ..Note::default()
            });
            let mut g = Fake::on_screen(7, 0);
            slot_a.set_peer_nat(peer);
            slot_a.offer(pair_link_cfg(2, 0xF000));
            let step =
                |g: &mut Fake, ui: &mut NetUi, np: &mut NetPlay, na: &mut FakeNet, us: u64| {
                    clock.set(clock.get() + us);
                    g.step_net(ui, np, na, 0)
                };
            for _ in 0..50 {
                step(&mut g, &mut ui, &mut np_a, &mut na, 100_000);
            }
            assert_eq!(np_a.restarts().count, 0, "{tag}: 5 秒で作り直した");
            if early {
                // 両方が直接接続できない型と分かった: 作り直しを待たずに出す。手順は止めない（セッションは動き続ける）。
                assert_eq!(board.show() & SHOW_WAIT, 0, "{tag}");
                assert_eq!(
                    board.show() & all_rows,
                    SHOW_TL0 | SHOW_TL1 | SHOW_TL2,
                    "{tag}"
                );
                assert_eq!(
                    read(board, ROW_TL0).map(|r| r.0).as_deref(),
                    Some(F4[0]),
                    "{tag}"
                );
            } else {
                assert_ne!(board.show() & SHOW_WAIT, 0, "{tag}");
                assert_eq!(board.show() & all_rows, 0, "{tag}");
                assert_eq!(
                    read(board, ROW_WAIT),
                    Some((
                        "接続コードで待機中 0:42 残り 29:18".to_string(),
                        RowStyle::Normal
                    )),
                    "{tag}"
                );
            }
            let mut t = 0;
            while np_a.restarts().count == 0 && t < 300 {
                step(&mut g, &mut ui, &mut np_a, &mut na, 100_000);
                t += 1;
            }
            assert_eq!(np_a.restarts().count, 1, "{tag}: 作り直さない");
            step(&mut g, &mut ui, &mut np_a, &mut na, 100_000);
            assert_eq!(board.show() & SHOW_WAIT, 0, "{tag}: 帯の 1 行が残った");
            let want_mask = rows.iter().take(want.len()).fold(0, |m, (bit, _)| m | bit);
            assert_eq!(board.show() & all_rows, want_mask, "{tag}");
            for (i, w) in want.iter().enumerate() {
                assert_eq!(
                    read(board, rows[i].1),
                    Some((w.to_string(), RowStyle::Warn)),
                    "{tag}: {} 行目",
                    i + 1
                );
            }
            slot_b.offer(pair_link_cfg(1, 1));
            let mut connected = false;
            for _ in 0..600 {
                step(&mut g, &mut ui, &mut np_a, &mut na, FRAME_US_FOR_TEST);
                np_b.tend(&mut nb);
                if matches!(np_a.ui_conn(0).0, Conn::Connected { .. }) {
                    connected = true;
                    break;
                }
            }
            assert!(connected, "{tag}: 相手が動き出してもつながらない");
            step(&mut g, &mut ui, &mut np_a, &mut na, FRAME_US_FOR_TEST);
            let (t0, s0) = read(board, ROW_TL0).unwrap_or_default();
            assert!(t0.starts_with("接続しました"), "{tag}: {t0}");
            assert_eq!(s0, RowStyle::Normal, "{tag}");
            assert_eq!(
                board.show() & (SHOW_TL1 | SHOW_TL2 | SHOW_TL3 | SHOW_TL4),
                0,
                "{tag}: 警告の続きの行が残った"
            );
            assert_eq!(board.show() & SHOW_WAIT, 0);
        }
        // 相手の接続を待つのを取りやめた後（見張りの状態は`Idle`）: 古い作り直しの数（`Some(stall)`）が残っていても、何も出ない。
        let (mut ui, link, board) = new_ui();
        link.publish(&Note {
            seq: 1,
            state: higanr_core::net_menu::CodeState::Idle,
            event: higanr_core::net_menu::Event::Cancelled,
            since: 1_000_000 - 42,
            ..Note::default()
        });
        let mut g = Fake::on_screen(7, 0);
        let stale = UiFacts {
            stall: Some(higanr_core::net_menu::Stall {
                why: higanr_core::net_menu::StallWhy::NothingReceived,
                fail: higanr_core::nat::FailKind::BothSymmetric,
            }),
            ..facts()
        };
        g.step(&mut ui, &stale, 0);
        assert_eq!(
            board.show() & (SHOW_WAIT | all_rows),
            0,
            "取りやめた後にも左上の行が残った"
        );
    }

    #[test]
    fn cancelling_a_stalled_wait_does_not_leave_a_false_warning_for_the_next_wait() {
        use crate::netplay::LinkSlot;
        use crate::netplay::fake::FakeNet;
        use higanr_core::candidate::Candidates;
        use higanr_core::net_menu::{CodeState, Event};
        for (linking, fresh, ev) in [
            (CodeState::Linking, CodeState::Offered, Event::Made),
            (
                CodeState::RoomLinking,
                CodeState::RoomWaiting,
                Event::RoomPeer,
            ),
        ] {
            let (a, b) = (
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
                std::net::SocketAddr::from(([127, 0, 0, 1], 2)),
            );
            let (mut na, _nb, clock) = FakeNet::pair(a, b, 1_000);
            let slot_a = Arc::new(LinkSlot::new());
            let mut np_a =
                NetPlay::with_code_slot(Arc::clone(&slot_a), code_play_config(), &mut na);
            let (mut ui, link, board) = new_ui();
            let note = |state: CodeState, event: Event, seq: u32| Note {
                seq,
                event,
                state,
                since: 1_000_000 - 42,
                ..Note::default()
            };
            link.publish(&note(linking, ev, 1));
            let mut g = Fake::on_screen(7, 0);
            slot_a.offer(pair_link_cfg(2, 0xF000));
            let step =
                |g: &mut Fake, ui: &mut NetUi, np: &mut NetPlay, na: &mut FakeNet, us: u64| {
                    clock.set(clock.get() + us);
                    g.step_net(ui, np, na, 0)
                };
            let mut t = 0;
            while np_a.restarts().count == 0 && t < 300 {
                step(&mut g, &mut ui, &mut np_a, &mut na, 100_000);
                t += 1;
            }
            assert_eq!(np_a.restarts().count, 1, "{linking:?}: 作り直さない");
            step(&mut g, &mut ui, &mut np_a, &mut na, 100_000);
            assert_eq!(
                board.show() & (SHOW_TL0 | SHOW_TL1),
                SHOW_TL0 | SHOW_TL1,
                "{linking:?}: 取りやめる前の 2 行が出ない"
            );
            link.publish(&note(CodeState::Idle, Event::Cancelled, 2));
            slot_a.offer(higanr_core::link::Config {
                peers: Candidates::EMPTY,
                ..pair_link_cfg(2, 0xF001)
            });
            for _ in 0..250 {
                step(&mut g, &mut ui, &mut np_a, &mut na, 100_000);
            }
            assert_eq!(
                np_a.restarts().count,
                0,
                "{linking:?}: 候補の無いセッションの作り直しが数えられる"
            );
            link.publish(&Note {
                since: 1_000_000,
                ..note(fresh, ev, 3)
            });
            for _ in 0..3 {
                step(&mut g, &mut ui, &mut np_a, &mut na, 100_000);
            }
            assert_ne!(board.show() & SHOW_WAIT, 0, "{fresh:?}: 帯の 1 行が出ない");
            assert_eq!(
                board.show() & (SHOW_TL0 | SHOW_TL1),
                0,
                "{fresh:?}: 偽の警告の 2 行が出た"
            );
        }
    }

    const FRAME_US_FOR_TEST: u64 = crate::netplay::fake::FRAME_US;

    // ================= 対戦の成立（READY）=================

    fn found_facts(ready: bool) -> UiFacts {
        UiFacts {
            conn: Conn::Found {
                seat: Seat::P1,
                shape: Some((Mode::Rollback, 1)),
                ready,
                left_secs: 599,
            },
            net: NetPhase::Open,
            ..facts()
        }
    }

    #[test]
    fn the_side_with_the_menu_open_when_found_has_chosen_and_waits() {
        let (mut ui, _l, b) = new_ui();
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        assert!(ui.menu_active());
        let o = g.step(&mut ui, &found_facts(false), 0);
        assert!(o.ready, "メニューを開いていた側は選んだ");
        assert_eq!(ui.stats().readies, 1);
        let o = g.step(&mut ui, &found_facts(true), 0);
        assert!(!o.ready, "2 度頼まない");
        assert!(!ui.menu_active(), "メニューは閉じる");
        let before = g.feeds.len();
        for w in [DOWN, 0, UP, 0, Z, 0, 0x0004, 0] {
            let o = g.step(&mut ui, &found_facts(true), w);
            assert_eq!(o.action, Some(Action::Menu));
            assert!(!o.ready);
        }
        assert!(
            g.feeds[before..].iter().all(|f| *f == [0, 0, 0]),
            "待っている間に語が流れた: {:?}",
            &g.feeds[before..]
        );
        assert_eq!(ui.stats().readies, 1);
        assert_eq!(
            row_text(b, ROW_TL0),
            "相手が「妖怪 対 妖怪」を選ぶのを待っています（閉じるまで 9:59）"
        );
        g.step(&mut ui, &found_facts(true), X);
        assert!(ui.menu_active());
        assert_eq!(row_text(b, 0), "対戦から抜けますか？");
        g.step(&mut ui, &found_facts(true), 0);
        g.step(&mut ui, &found_facts(true), UP);
        g.step(&mut ui, &found_facts(true), 0);
        let o = g.step(&mut ui, &found_facts(true), Z);
        assert!(o.leave, "はい: 抜ける");
    }

    #[test]
    fn a_side_elsewhere_when_found_keeps_playing_until_it_chooses() {
        let (mut ui, _l, b) = new_ui();
        let mut title = Fake::on_screen(1, 0);
        let o = title.step(&mut ui, &found_facts(false), 0);
        assert!(!o.ready, "別の画面にいた側は選んでいない");
        assert_eq!(
            row_text(b, ROW_TL0),
            "相手が見つかりました（閉じるまで 9:59）"
        );
        assert_eq!(row_text(b, ROW_TL1), "「妖怪 対 妖怪」を選んでください。");
        let mut story = Fake::on_match_screen(0);
        story.scenes = (2, 0);
        assert_eq!(
            story.step(&mut ui, &found_facts(false), Z),
            UiOut::default()
        );
        assert_ne!(b.show() & SHOW_TL0, 0);
        assert_eq!((story.local_reads, story.feeds.len()), (0, 0));
        for screen in [7, 8, 16] {
            let mut g = Fake::on_screen(screen, 0);
            for w in [Z, 0, X, 0] {
                let o = g.step(&mut ui, &found_facts(false), w);
                assert_eq!(o, UiOut::default(), "screen {screen}");
            }
            assert_eq!(
                (g.local_reads, g.stash_calls, g.feeds.len()),
                (0, 0, 0),
                "screen {screen}"
            );
        }
        let mut g = Fake::on_match_screen(0);
        g.step(&mut ui, &found_facts(false), Z);
        g.step(&mut ui, &found_facts(false), 0);
        g.step(&mut ui, &found_facts(false), X);
        assert_eq!(g.feeds, vec![[Z, 0, Z], [0, 0, 0], [X, 0, X]]);
        assert!(
            !ui.menu_active(),
            "取消で確かめの枠を出さない（まだ選んでいない）"
        );
        assert_eq!(ui.stats().readies, 0);
        let mut g = Fake::on_match_screen(4);
        let o = g.step(&mut ui, &found_facts(false), Z);
        assert!(o.ready);
        assert_eq!(g.feeds, vec![[0, 0, 0]], "決定が漏れた");
        assert!(!ui.menu_active());
        assert_eq!((ui.stats().readies, ui.stats().opened), (1, 0));
    }

    #[test]
    fn the_found_moment_with_the_menu_closed_does_not_choose() {
        let (mut ui, _l, _b) = new_ui();
        let mut g = Fake::on_match_screen(2);
        g.step(&mut ui, &facts(), 0);
        let o = g.step(&mut ui, &found_facts(false), 0);
        assert!(!o.ready);
        assert_eq!(ui.stats().readies, 0);
    }

    #[test]
    fn the_unmatched_reason_is_shown_on_every_scene() {
        for (why, want) in [
            (
                NoMatch::TimedOut,
                "対戦が始まらないまま10分たったので、接続を閉じました。",
            ),
            (
                NoMatch::PeerHasNoGate,
                "相手のHiganRToolが古いか、ゲーム内のメニューを使っていないので接続を閉じました。",
            ),
            (
                NoMatch::PeerLeft,
                "相手が対戦を始めないまま抜けたので、接続を閉じました。",
            ),
        ] {
            let unmatched = UiFacts {
                conn: Conn::Unmatched(why),
                net: NetPhase::Open,
                ..facts()
            };
            for scenes in [(2u32, 0u32), (SCENE_MENU, 0)] {
                let (mut ui, _l, b) = new_ui();
                let mut g = Fake::on_screen(1, 0);
                g.scenes = scenes;
                g.step(&mut ui, &unmatched, 0);
                assert_ne!(b.show() & SHOW_TL0, 0, "{why:?} {scenes:?}");
                assert_eq!(row_text(b, ROW_TL0), want, "{why:?} {scenes:?}");
                assert_eq!(row_text(b, ROW_TL1), T_AGAIN.ja, "{why:?} {scenes:?}");
            }
        }
    }

    #[test]
    fn the_other_version_line_is_shown_on_every_scene() {
        let other = UiFacts {
            conn: Conn::OtherVersion { theirs: 0 },
            net: NetPhase::Open,
            ..facts()
        };
        let want = format!(
            "相手とHiganRToolのバージョンが違います（自分 {}・相手 0）。",
            higanr_core::wire::PROTO_VER
        );
        for scenes in [(2u32, 0u32), (SCENE_MENU, 0)] {
            let (mut ui, _l, b) = new_ui();
            let mut g = Fake::on_screen(1, 0);
            g.scenes = scenes;
            g.step(&mut ui, &other, 0);
            assert_ne!(b.show() & SHOW_TL0, 0, "{scenes:?}");
            assert_eq!(row_text(b, ROW_TL0), want, "{scenes:?}");
            assert_eq!(
                row_text(b, ROW_TL1),
                "相手と同じzipのHiganRToolを使ってください。",
                "{scenes:?}"
            );
        }
    }

    /// 組み上がりの検査の2台（偽のネットワーク・偽のゲーム・メニュー）。AはMatchの画面で入口のメニューを開いて待つ側。Bは別の画面にいる側。
    struct Rig {
        na: crate::netplay::fake::FakeNet,
        nb: crate::netplay::fake::FakeNet,
        clock: std::rc::Rc<std::cell::Cell<u64>>,
        np_a: NetPlay,
        np_b: NetPlay,
        ui_a: NetUi,
        ui_b: NetUi,
        ga: Fake,
        gb: Fake,
        slot_a: Arc<crate::netplay::LinkSlot>,
        slot_b: Arc<crate::netplay::LinkSlot>,
    }

    type RigOut = (Action, UiOut, Action, UiOut);

    impl Rig {
        fn new(gate: bool, ga: Fake, gb: Fake) -> Rig {
            Rig::new_sides(gate, gate, ga, gb)
        }

        /// ゲートの有無を側ごとに決める（ゲートの無い相手 = 古いバージョンの`code_side = game`か、同じバージョンの`offer` / `answer`の模型）。
        fn new_sides(gate_a: bool, gate_b: bool, ga: Fake, gb: Fake) -> Rig {
            use crate::netplay::fake::FakeNet;
            let (a, b) = (
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
                std::net::SocketAddr::from(([127, 0, 0, 1], 2)),
            );
            let (mut na, mut nb, clock) = FakeNet::pair(a, b, 1_000);
            let (slot_a, slot_b) = (
                Arc::new(crate::netplay::LinkSlot::new()),
                Arc::new(crate::netplay::LinkSlot::new()),
            );
            let cfg = |gate| crate::netplay::Config {
                match_gate: gate,
                ..code_play_config()
            };
            let np_a = NetPlay::with_code_slot(Arc::clone(&slot_a), cfg(gate_a), &mut na);
            let np_b = NetPlay::with_code_slot(Arc::clone(&slot_b), cfg(gate_b), &mut nb);
            let (ui_a, _la, _ba) = new_ui();
            let (ui_b, _lb, _bb) = new_ui();
            Rig {
                na,
                nb,
                clock,
                np_a,
                np_b,
                ui_a,
                ui_b,
                ga,
                gb,
                slot_a,
                slot_b,
            }
        }

        fn tick_by(&mut self, wa: u16, wb: u16, step_us: u64) -> RigOut {
            let (aa, oa) = self
                .ga
                .step_net(&mut self.ui_a, &mut self.np_a, &mut self.na, wa);
            let (ab, ob) = self
                .gb
                .step_net(&mut self.ui_b, &mut self.np_b, &mut self.nb, wb);
            self.clock.set(self.clock.get() + step_us);
            (aa, oa, ab, ob)
        }

        fn tick(&mut self, wa: u16, wb: u16) -> RigOut {
            self.tick_by(wa, wb, crate::netplay::fake::FRAME_US)
        }

        fn link_up(&mut self) -> u32 {
            self.slot_a.offer(pair_link_cfg(2, 0xF000));
            self.slot_b.offer(pair_link_cfg(1, 1));
            for t in 0..250u32 {
                self.tick(0, 0);
                if self.np_a.connected().is_some() && self.np_b.connected().is_some() {
                    return t;
                }
            }
            panic!("つながらない");
        }

        /// 成立の前に送ってはいけないtype（INPUT 3・CHECK 4・SELECT 11・MENU 16・STEP_REQ 17・STEP 18）を両者が送った数。
        fn post_match_sent(&self) -> u32 {
            [3usize, 4, 11, 16, 17, 18]
                .iter()
                .map(|&t| self.na.types[t] + self.nb.types[t])
                .sum()
        }
    }

    const TYPE_READY: usize = 20;
    const TYPE_BYE: usize = 15;

    #[test]
    fn the_match_starts_only_when_both_choose_and_nothing_else_crosses_before() {
        let mut r = Rig::new(true, Fake::on_match_screen(4), Fake::on_screen(7, 0));
        let (_, _, ab, _) = r.tick(Z, Z);
        assert_eq!(
            ab,
            Action::Local,
            "つながる前のローカルの画面 7 をネットワークが握った"
        );
        r.tick(0, 0);
        assert!(r.ui_a.menu_active());
        r.link_up();
        assert_eq!(r.ui_a.stats().readies, 1, "メニューを開いていた A は選んだ");
        assert_eq!(r.np_a.phase(), crate::netplay::Phase::Unconnected);
        assert!(matches!(
            r.np_a.ui_conn(0).0,
            Conn::Found { ready: true, .. }
        ));
        r.tick(DOWN, 0);
        r.tick(0, 0);
        assert_eq!(
            r.ga.feeds[r.ga.feeds.len() - 2..],
            [[0, 0, 0], [0, 0, 0]],
            "選んで待つ A の Match の画面に語が流れた"
        );
        let reads_before = r.gb.local_reads;
        for i in 0..300u32 {
            let w = if i % 2 == 0 { Z } else { 0 };
            let (_, oa, ab, ob) = r.tick(0, w);
            assert_eq!(
                ab,
                Action::Local,
                "tick {i}: B の画面 7 をネットワークが握った"
            );
            assert!(!ob.ready && !oa.ready);
        }
        assert!(r.gb.feeds.is_empty(), "B のゲームへ feed した");
        assert_eq!(r.gb.local_reads, reads_before, "B の語を読んだ");
        assert_eq!(r.np_a.phase(), crate::netplay::Phase::Unconnected);
        assert_eq!(r.np_b.phase(), crate::netplay::Phase::Unconnected);
        assert_eq!(r.post_match_sent(), 0, "成立の前に選択・試合の通信を送った");
        assert!(
            r.na.types[TYPE_READY] >= 5,
            "A の READY {}",
            r.na.types[TYPE_READY]
        );
        assert_eq!(
            r.nb.types[TYPE_READY], 0,
            "選んでいない B が READY を送った"
        );
        assert!(matches!(
            r.np_b.ui_conn(0).0,
            Conn::Found { ready: false, .. }
        ));
        let st = crate::net_watch::NetStatus::new();
        st.publish(&r.np_a, false);
        let line = st.snapshot().gate_line().expect("ゲートの行が無い");
        assert!(
            line.starts_with("対戦の成立: 自分 選んだ / 相手 まだ / READY 送った "),
            "{line}"
        );
        r.gb.view = Some(MenuView {
            screen: 6,
            sub: 1,
            trans: 0,
            cursor: 4,
            ..MenuView::default()
        });
        let (_, _, _, ob) = r.tick(0, Z);
        assert!(ob.ready);
        assert!(
            r.nb.types[TYPE_READY] >= 1,
            "選んだ tick に READY を出していない"
        );
        for _ in 0..5 {
            r.tick(0, 0);
        }
        assert_eq!(r.np_a.phase(), crate::netplay::Phase::Selecting);
        assert_eq!(r.np_b.phase(), crate::netplay::Phase::Selecting);
        assert!(r.ui_a.stats().auto_presses > 0 && r.ui_b.stats().auto_presses > 0);
        // 成立した後も相手の成立後の通信が来るまで自分の側からREADYを送り直す。重複のREADYで自動の前進を2度始めない。
        for _ in 0..200 {
            r.tick(0, 0);
        }
        let dup = r
            .np_b
            .connected()
            .and_then(|c| c.match_gate())
            .map(|g| g.counts().ready_dup)
            .unwrap();
        assert!(dup >= 1, "重複の READY が届いていない（検査の前提）");
        assert_eq!(
            (r.ui_a.stats().auto_starts, r.ui_b.stats().auto_starts),
            (1, 1)
        );
        // 正の対照: 上の「成立の前は送らない」の0が、この模型ではいつまでも0のまま（送る経路が無い）なのかを区別する。
        // 成立の後に両者の偽のゲームを難易度の画面（7）へ進めると選択の同期のMENUが送られ始める。
        assert_eq!(r.post_match_sent(), 0, "画面を動かす前（検査の前提）");
        for g in [&mut r.ga, &mut r.gb] {
            g.view = Some(MenuView {
                screen: 7,
                sub: 1,
                trans: 0,
                cursor: 0,
                ..MenuView::default()
            });
        }
        for _ in 0..6 {
            r.tick(0, 0);
        }
        assert!(
            r.na.types[16] > 0 && r.nb.types[16] > 0,
            "成立の後に画面 7 へ来ても MENU を送らない（A {}・B {}）",
            r.na.types[16],
            r.nb.types[16]
        );
        assert!(r.post_match_sent() > 0);
        for _ in 0..300 {
            r.tick(0, 0);
        }
        assert_eq!(r.np_a.end(), None);
        assert_eq!(r.np_b.end(), None);
        for np in [&r.np_a, &r.np_b] {
            assert_eq!(np.connected().unwrap().match_gate().unwrap().closed(), None);
        }
    }

    #[test]
    fn an_unmatched_connection_closes_after_ten_minutes_and_says_why() {
        let mut r = Rig::new(true, Fake::on_match_screen(4), Fake::on_screen(1, 0));
        r.tick(Z, 0);
        r.tick(0, 0);
        r.link_up();
        let t0 = r.clock.get();
        let left = |np: &NetPlay| {
            np.connected()
                .and_then(|c| c.match_gate())
                .map(|g| g.left_ms((t0 / 1_000) as u32))
                .unwrap()
        };
        assert!(
            left(&r.np_b) < left(&r.np_a),
            "B の時計が先に始まっている（検査の前提）"
        );
        let wait_us = u64::from(higanr_core::match_gate::MATCH_WAIT_MS) * 1_000;
        let ended = |r: &Rig| {
            r.np_a.phase() == crate::netplay::Phase::Ended
                || r.np_b.phase() == crate::netplay::Phase::Ended
        };
        let mut ended_at = None;
        for _ in 0..10_000 {
            let step = if r.clock.get() - t0 + 2_000_000 < wait_us {
                500_000
            } else {
                1_000
            };
            r.tick_by(0, 0, step);
            if ended(&r) {
                ended_at = Some(r.clock.get() - t0);
                break;
            }
        }
        let at = ended_at.expect("10 分を過ぎても閉じない");
        // 起点t0は`link_up`が返った後の時刻。セッションが接続した時刻（ゲートの時計の起点）はその1〜2 tick前になる。
        // 1 msずつ進めるとその差（2フレーム以内）だけ10分より早く見える。
        assert!(
            at + 2 * crate::netplay::fake::FRAME_US >= wait_us && at <= wait_us + 1_000_000,
            "閉じたのは {} ms",
            at / 1_000
        );
        assert_eq!(
            r.np_b.end(),
            Some(crate::netplay::End::Unmatched(NoMatch::TimedOut)),
            "先に切れるのは B"
        );
        assert_eq!(r.np_b.ui_conn(0).0, Conn::Unmatched(NoMatch::TimedOut));
        assert!(r.nb.types[TYPE_BYE] >= 1, "BYE を送っていない");
        assert_eq!(r.post_match_sent(), 0);
        // AはBのBYE(取消)で終わる。成立の前のBYE(取消)は「相手が閉じた」で、遊んだ後の「つながりが終わりました。」と見分ける。
        for _ in 0..10 {
            r.tick(0, 0);
        }
        assert_eq!(
            r.np_a.end(),
            Some(crate::netplay::End::Unmatched(NoMatch::PeerLeft))
        );
        assert_eq!(r.np_a.ui_conn(0).0, Conn::Unmatched(NoMatch::PeerLeft));
        assert_eq!(
            crate::net_watch::end_code(r.np_a.end()),
            (10, ByeReason::Cancelled as u32)
        );
        assert_eq!(crate::net_watch::end_code(r.np_b.end()), (8, 0));
        r.ga.view = Some(MenuView {
            screen: 1,
            sub: 1,
            trans: 0,
            cursor: 0,
            ..MenuView::default()
        });
        r.tick(0, 0);
        let row = |ui: &NetUi| {
            let mut buf = [0u8; MAX_TEXT_UTF8];
            ui.board
                .read_row(ROW_TL0, &mut buf)
                .map(|(t, _)| t.to_string())
        };
        assert_eq!(
            row(&r.ui_a).as_deref(),
            Some("相手が対戦を始めないまま抜けたので、接続を閉じました。")
        );
        assert_eq!(
            row(&r.ui_b).as_deref(),
            Some("対戦が始まらないまま10分たったので、接続を閉じました。")
        );
        for _ in 0..3 {
            r.tick_by(0, 0, 1_000_000);
        }
        let now_ms = (r.clock.get() / 1_000) as u32;
        for np in [&r.np_a, &r.np_b] {
            assert!(np.relink_due(now_ms, (SCENE_MENU, 0), crate::netplay::RELINK_GRACE_MS));
        }
    }

    #[test]
    fn an_ended_connection_is_torn_down_outside_the_match_and_the_menu_connects_again() {
        use crate::netplay::{Phase, RELINK_GRACE_MS};
        let mut r = Rig::new(true, Fake::on_match_screen(4), Fake::on_match_screen(4));
        r.tick(Z, Z);
        r.tick(0, 0);
        r.link_up();
        for _ in 0..10 {
            r.tick(0, 0);
        }
        assert_eq!(r.np_b.phase(), Phase::Selecting, "検査の前提: 成立した");
        let session = r.np_b.code_state().session;
        assert_eq!(session, 9, "検査の前提: `pair_link_cfg` の session");
        let now_ms = |r: &Rig| (r.clock.get() / 1_000) as u32;
        assert!(
            !r.np_b.relink_due(now_ms(&r), (SCENE_MENU, 0), 0),
            "つながっているセッションを片づけてよいと言った"
        );
        assert_eq!(r.np_b.spent_session(), Some(9));
        r.np_a.close(ByeReason::Cancelled);
        for _ in 0..5 {
            r.tick(0, 0);
        }
        assert_eq!(
            r.np_b.end(),
            Some(crate::netplay::End::PeerBye(ByeReason::Cancelled))
        );
        let ended_at = now_ms(&r);
        assert!(
            !r.np_b
                .relink_due(ended_at, (SCENE_MENU, 0), RELINK_GRACE_MS),
            "BYE を届け直す待ちの間に片づけてよいと言った"
        );
        while now_ms(&r) < ended_at + RELINK_GRACE_MS + 50 {
            r.tick_by(0, 0, 100_000);
        }
        let t = now_ms(&r);
        assert!(
            !r.np_b.relink_due(t, (2, 0), RELINK_GRACE_MS),
            "試合の中で片づけてよいと言った"
        );
        assert!(
            !r.np_b.relink_due(t, (SCENE_MENU, 2), RELINK_GRACE_MS),
            "試合へ移る tick に片づけてよいと言った"
        );
        assert!(r.np_b.relink_due(t, (SCENE_MENU, 0), RELINK_GRACE_MS));
        let link_b = Arc::clone(r.ui_b.link());
        link_b.publish(&Note {
            seq: 41,
            event: Event::PeerRead,
            state: CodeState::Linking,
            ..Note::default()
        });
        r.tick(0, 0);
        link_b.send(Cmd::Paste);
        let last = r.np_b.ui_conn(0).0;
        assert_eq!(last, Conn::Finished);
        r.ui_b.after_teardown(last);
        let mark = link_b.order_mark();
        assert_eq!(r.ui_b.stats().teardowns, 1);
        let mut g = Fake::on_screen(1, 0);
        let b = r.ui_b.board;
        g.step(&mut r.ui_b, &facts(), 0);
        assert_eq!(row_text(b, ROW_TL0), "接続が終わりました。");
        assert_eq!(row_text(b, ROW_TL1), T_AGAIN.ja);
        let mut m = Fake::on_match_screen(4);
        m.step(&mut r.ui_b, &facts(), Z);
        m.step(&mut r.ui_b, &facts(), 0);
        assert!(r.ui_b.menu_active());
        let rows = menu_rows(b);
        assert!(rows.iter().any(|l| l.contains("コード作成")), "{rows:?}");
        assert!(
            !rows
                .iter()
                .any(|l| l.contains("待機をやめる") || l.contains("返答コード入力")),
            "前の見張りの状態でメニューを描いた: {rows:?}"
        );
        assert!(
            !rows.iter().any(|l| l.contains("接続しています")),
            "{rows:?}"
        );
        let o = m.step(&mut r.ui_b, &facts(), Z);
        assert!(
            o.open_net,
            "片づけた後の「作る」がネットワークを開く頼みにならない"
        );
        link_b.reset_for_next_watcher(mark);
        assert_eq!(link_b.take_order(), Some(Order::Create));
        assert_eq!(link_b.take_order(), None);
        let n = link_b.note().unwrap();
        assert_eq!(
            (n.event, n.state, n.seq),
            (Event::None, CodeState::Idle, 42)
        );
        m.step(&mut r.ui_b, &facts(), X);
        for _ in 0..3 {
            m.step(&mut r.ui_b, &facts(), 0);
        }
        assert!(!r.ui_b.menu_active());
        g.step(&mut r.ui_b, &facts(), 0);
        assert_eq!(b.show() & (SHOW_TL0 | SHOW_TL1), 0, "終わりの行が残った");
    }

    #[test]
    fn a_gateless_peer_is_closed_three_seconds_after_its_first_menu_and_says_why() {
        let mut r = Rig::new_sides(true, false, Fake::on_match_screen(4), Fake::on_screen(7, 0));
        r.tick(Z, 0);
        r.tick(0, 0);
        r.link_up();
        assert!(
            r.np_b.connected().unwrap().match_gate().is_none(),
            "B はゲートなし"
        );
        // BのMENUの1通目の時刻は、それを送ったtickの始まり（Aが受けたのはその後）。
        // Aが閉じた時刻は、閉じたtickの終わり（閉じたのはその前）。
        // この差は3秒を下回らない。
        let mut first_menu_at = None;
        let mut a_ended_at = None;
        for _ in 0..600 {
            let before = r.clock.get();
            r.tick(0, 0);
            if first_menu_at.is_none() && r.nb.types[16] > 0 {
                first_menu_at = Some(before);
            }
            if a_ended_at.is_none() && r.np_a.phase() == crate::netplay::Phase::Ended {
                a_ended_at = Some(r.clock.get());
            }
            if a_ended_at.is_some() && r.np_b.phase() == crate::netplay::Phase::Ended {
                break;
            }
        }
        let menu = first_menu_at.expect("ゲートの無い B が MENU を送らない（検査の前提）");
        let ended = a_ended_at.expect("ゲートの無い相手の MENU を受けても A が閉じない");
        let no_gate_us = u64::from(higanr_core::match_gate::NO_GATE_MS) * 1_000;
        let took = ended - menu;
        assert!(
            took >= no_gate_us && took <= no_gate_us + 3 * crate::netplay::fake::FRAME_US,
            "B の MENU の 1 通目から {} ms で閉じた",
            took / 1_000
        );
        assert_eq!(
            r.np_a.end(),
            Some(crate::netplay::End::Unmatched(NoMatch::PeerHasNoGate))
        );
        assert_eq!(r.np_a.ui_conn(0).0, Conn::Unmatched(NoMatch::PeerHasNoGate));
        assert_eq!(crate::net_watch::end_code(r.np_a.end()), (9, 0));
        assert!(r.na.types[TYPE_BYE] >= 1, "BYE を送っていない");
        assert_eq!(
            [3usize, 4, 11, 16, 17, 18]
                .iter()
                .map(|&t| r.na.types[t])
                .sum::<u32>(),
            0,
            "A が成立後の通信を送った"
        );
        assert_eq!(
            r.np_b.end(),
            Some(crate::netplay::End::PeerBye(ByeReason::Cancelled))
        );
        assert_eq!(r.np_b.ui_conn(0).0, Conn::Finished);
        r.ga.view = Some(MenuView {
            screen: 1,
            sub: 1,
            trans: 0,
            cursor: 0,
            ..MenuView::default()
        });
        r.tick(0, 0);
        let row = |i| {
            let mut buf = [0u8; MAX_TEXT_UTF8];
            r.ui_a
                .board
                .read_row(i, &mut buf)
                .map(|(t, _)| t.to_string())
        };
        assert_eq!(
            row(ROW_TL0).as_deref(),
            Some(
                "相手のHiganRToolが古いか、ゲーム内のメニューを使っていないので接続を閉じました。"
            )
        );
        assert_eq!(row(ROW_TL1).as_deref(), Some(T_AGAIN.ja));
        let st = crate::net_watch::NetStatus::new();
        st.publish(&r.np_a, false);
        let line = st.snapshot().gate_line().expect("ゲートの行が無い");
        assert!(line.contains("相手にゲートが無いので閉じた"), "{line}");
        assert!(line.contains("受けた 0 通"), "{line}");
    }

    #[test]
    fn a_cancel_after_the_match_is_still_a_peer_bye() {
        let mut r = Rig::new(true, Fake::on_match_screen(4), Fake::on_match_screen(4));
        r.tick(Z, 0);
        r.tick(0, 0);
        r.link_up();
        r.tick(0, Z);
        for _ in 0..5 {
            r.tick(0, 0);
        }
        assert!(r.np_a.connected().unwrap().matched());
        assert_eq!(r.np_a.phase(), crate::netplay::Phase::Selecting);
        r.np_b.close(ByeReason::Cancelled);
        for _ in 0..10 {
            r.tick(0, 0);
        }
        assert_eq!(
            r.np_a.end(),
            Some(crate::netplay::End::PeerBye(ByeReason::Cancelled))
        );
        assert_eq!(r.np_a.ui_conn(0).0, Conn::Finished);
        assert_eq!(crate::net_watch::end_code(r.np_a.end()), (2, 4));
    }

    #[test]
    fn without_the_gate_the_code_path_is_unchanged() {
        let mut r = Rig::new(false, Fake::on_match_screen(4), Fake::on_screen(7, 0));
        let (_, _, ab, _) = r.tick(Z, Z);
        assert_eq!(
            ab,
            Action::Menu,
            "ゲートの無い実行は、つながる前の画面 7 の決定を落とす"
        );
        assert_eq!(r.gb.last_feed(), Some([0, 0, 0]), "決定を落とした語");
        r.tick(0, 0);
        r.gb.view = Some(MenuView {
            screen: 6,
            sub: 1,
            trans: 0,
            cursor: 0,
            ..MenuView::default()
        });
        r.link_up();
        r.tick(0, 0);
        assert_eq!(r.np_a.phase(), crate::netplay::Phase::Selecting);
        assert_eq!(r.np_b.phase(), crate::netplay::Phase::Selecting);
        assert!(matches!(r.np_a.ui_conn(0).0, Conn::Connected { .. }));
        for _ in 0..300 {
            r.tick(0, 0);
        }
        assert_eq!(r.na.types[TYPE_READY] + r.nb.types[TYPE_READY], 0);
        assert!(r.np_a.connected().unwrap().match_gate().is_none());
    }

    /// 確保しない偽のゲーム（固定の配列だけ。確保の検査用。`Fake`は記録に`Vec`を使う）。
    struct Quiet {
        screen: u32,
        cursor: u32,
        word: u16,
        im2: InputState,
        fed: [u16; 3],
        typed: [u16; 8],
        typed_n: usize,
        gate: bool,
    }

    impl Game for Quiet {
        fn scenes(&mut self) -> (u32, u32) {
            (SCENE_MENU, 0)
        }
        fn text_gate(&mut self, open: bool) -> bool {
            self.gate = open;
            true
        }
        fn typed(&mut self, out: &mut [u16]) -> usize {
            let n = if self.gate {
                self.typed_n.min(out.len())
            } else {
                0
            };
            for (d, s) in out.iter_mut().zip(self.typed.iter()).take(n) {
                *d = *s;
            }
            self.typed_n = 0;
            n
        }
        fn local_input(&mut self, _seat: Seat) -> u16 {
            self.word
        }
        fn feed(&mut self, index: u32, word: u16) {
            if let Some(f) = self.fed.get_mut(index as usize) {
                *f = word;
            }
            if index == 2 {
                self.im2.feed(word);
            }
        }
        fn own_select(&mut self, _s: Seat, _r: Role) -> Select {
            unreachable!()
        }
        fn apply_agreed(&mut self, _p: Seat, _a: &Agreed) -> Result<(), BadValue> {
            unreachable!()
        }
        fn write_charge(&mut self, _p1: u8, _p2: u8) -> bool {
            unreachable!()
        }
        fn check_values(&mut self) -> (u16, u32, u8, u8) {
            unreachable!()
        }
        fn menu_view(&mut self) -> Option<MenuView> {
            Some(MenuView {
                screen: self.screen,
                sub: 1,
                trans: 0,
                cursor: self.cursor,
                ..MenuView::default()
            })
        }
        fn input_state(&mut self, _index: usize) -> InputState {
            self.im2
        }
    }

    #[test]
    fn the_game_thread_path_does_not_allocate() {
        let (mut ui, link, b) = new_ui();
        let s = spec();
        let mut g = Quiet {
            screen: 6,
            cursor: 4,
            word: 0,
            im2: InputState::new(),
            fed: [0; 3],
            typed: [0; 8],
            typed_n: 0,
            gate: false,
        };
        let notes = [
            Note::default(),
            Note {
                seq: 1,
                event: Event::Made,
                state: CodeState::Offered,
                code_len: 58,
                head: *b"AbCd",
                tail: *b"QrSt",
                since: 1_000_000 - 42,
                ..Note::default()
            },
            Note {
                seq: 2,
                event: Event::Broken,
                state: CodeState::Offered,
                paste_len: 80,
                ..Note::default()
            },
        ];
        let wait = facts();
        let conn = connected_facts();
        let found_no = found_facts(false);
        let found_yes = found_facts(true);
        let unmatched = UiFacts {
            conn: Conn::Unmatched(NoMatch::TimedOut),
            net: NetPhase::Open,
            ..facts()
        };
        let no_gate = UiFacts {
            conn: Conn::Unmatched(NoMatch::PeerHasNoGate),
            ..unmatched
        };
        let peer_left = UiFacts {
            conn: Conn::Unmatched(NoMatch::PeerLeft),
            ..unmatched
        };
        let other_ver = UiFacts {
            conn: Conn::OtherVersion { theirs: 0 },
            ..unmatched
        };
        let mut draws = 0usize;
        // 1回の巡りの中身。1周目は静的変数の初期化などを済ませる。2周目の確保の数を見る。
        let tick = |ui: &mut NetUi,
                    g: &mut Quiet,
                    f: &UiFacts,
                    screen: u32,
                    cursor: u32,
                    words: &[u16]| {
            g.screen = screen;
            g.cursor = cursor;
            for (i, &w) in words.iter().enumerate() {
                g.word = w;
                link.publish(&notes[i % notes.len()]);
                let _ = ui.tick(g, f);
            }
        };
        for round in 0..2 {
            let before = crate::test_alloc::count();
            tick(
                &mut ui,
                &mut g,
                &wait,
                6,
                4,
                &[Z, 0, Z, 0, DOWN, 0, Z, 0, DOWN, 0, 0x0080, 0, X, 0, 0],
            );
            ui.auto = None;
            ui.was_connected = false;
            for c in [4, 4, 3, 3, 2, 2, 1, 1, 0, 0] {
                tick(&mut ui, &mut g, &conn, 6, c, &[0, 0]);
            }
            ui.auto = None;
            ui.was_connected = true;
            tick(&mut ui, &mut g, &conn, 6, 0, &[X, 0, UP, 0, Z, 0, 0, 0]);
            tick(&mut ui, &mut g, &conn, 7, 0, &[DOWN, 0, X, 0, Z, 0, 0]);
            ui.was_found = false;
            tick(&mut ui, &mut g, &found_no, 6, 4, &[0, Z, 0]);
            tick(
                &mut ui,
                &mut g,
                &found_yes,
                6,
                4,
                &[0, DOWN, 0, X, 0, Z, 0, 0],
            );
            tick(&mut ui, &mut g, &found_no, 1, 0, &[0, 0]);
            tick(&mut ui, &mut g, &unmatched, 1, 0, &[0, 0]);
            tick(&mut ui, &mut g, &no_gate, 1, 0, &[0, 0]);
            tick(&mut ui, &mut g, &peer_left, 1, 0, &[0, 0]);
            tick(&mut ui, &mut g, &other_ver, 1, 0, &[0, 0]);
            for r in 0..6u32 {
                ui.hud(
                    true,
                    HudFacts {
                        latency: Some(1),
                        rtt_ms: Some(30 + r),
                        rewound: (r % 2 == 1).then_some(r % 4),
                        recommend: Some(1),
                        auto: r % 3 == 0,
                        format: (r % 2 == 0).then_some(Mode::Rollback),
                        delay_only: r % 4 == 0,
                        names: Some(HudNames::of(
                            [Seat::P1, Seat::P2][(r / 2) as usize % 2],
                            Name::new("synth-self"),
                            Name::new("synth-peer"),
                        )),
                        ..HudFacts::default()
                    },
                );
                draws += draw_board(b, &s, 640, 480, &mut |v| {
                    assert!(v.len() % 6 == 0);
                });
            }
            ui.hud(false, HudFacts::default());
            tick(&mut ui, &mut g, &wait, 6, 4, &[Z, 0, 0]);
            draws += draw_board(b, &s, 640, 480, &mut |v| {
                assert!(v.len() % 6 == 0);
            });
            tick(&mut ui, &mut g, &wait, 6, 0, &[X, 0, 0]);
            let after = crate::test_alloc::count();
            if round == 1 {
                assert_eq!(after, before, "ゲームのスレッドの経路で確保した");
            }
        }
        assert!(draws >= 20, "盤を描く経路を通っていない（{draws}）");
        let st = ui.stats();
        assert!(
            st.opened > 0
                && st.orders_sent > 0
                && st.confirms > 0
                && st.auto_presses > 0
                && st.readies > 0,
            "経路を通っていない {st:?}"
        );
    }

    #[test]
    fn the_room_input_path_does_not_allocate() {
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b)
            .with_room(RoomItem::On(RoomBuf::from_name("init-name").unwrap()));
        let s = spec();
        let mut g = Quiet {
            screen: 6,
            cursor: 4,
            word: 0,
            im2: InputState::new(),
            fed: [0; 3],
            typed: [0; 8],
            typed_n: 0,
            gate: false,
        };
        let f = facts();
        let put = |g: &mut Quiet, text: &str| {
            for (d, u) in g.typed.iter_mut().zip(text.encode_utf16()) {
                *d = u;
            }
            g.typed_n = text.encode_utf16().count().min(8);
        };
        let mut draws = 0usize;
        for round in 0..2 {
            let before = crate::test_alloc::count();
            // 開く → 設定「ルーム名」を決定して入力へ（決定のキーを離すまでの間にEnterの文字が来ても読み捨てる）。
            // ルーム名が設定済みだと「ルームマッチ」は入力を経ずに始まるので、入力へ入る経路は「ルーム名」から通す。
            for w in [Z, 0, DOWN, 0, DOWN, 0, Z, 0, 0, 0, 0, 0, 0, 0, 0] {
                g.word = w;
                let _ = ui.tick(&mut g, &f);
            }
            put(&mut g, "ＡＢc-_\u{8}.あ");
            for w in [0, 0] {
                g.word = w;
                let _ = ui.tick(&mut g, &f);
            }
            put(&mut g, "\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}");
            let _ = ui.tick(&mut g, &f);
            put(&mut g, "\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}\r");
            let _ = ui.tick(&mut g, &f);
            draws += draw_board(b, &s, 640, 480, &mut |v| {
                assert!(v.len() % 6 == 0);
            });
            put(&mut g, "room-1\r");
            let _ = ui.tick(&mut g, &f);
            for w in [0, 0, Z, 0, 0, 0, 0, 0, 0, 0, 0, ESC, 0, 0] {
                g.word = w;
                let _ = ui.tick(&mut g, &f);
            }
            draws += draw_board(b, &s, 640, 480, &mut |v| {
                assert!(v.len() % 6 == 0);
            });
            for w in [UP, 0, Z, 0, 0] {
                g.word = w;
                let _ = ui.tick(&mut g, &f);
            }
            for w in [X, 0, 0] {
                g.word = w;
                let _ = ui.tick(&mut g, &f);
            }
            let after = crate::test_alloc::count();
            if round == 1 {
                assert_eq!(after, before, "ルーム名の入力の経路で確保した");
            }
        }
        assert!(draws >= 2);
        assert_eq!(
            link.room_name(),
            RoomBuf::from_name("room-1"),
            "決めた名が入れ物に届いていない"
        );
        assert_eq!(link.take_order(), Some(Order::Room));
    }

    /// 設定で決定したルーム名の記録（本物は別のスレッドがiniへ書く`ini_save::save_room`）。
    static SAVED_ROOMS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    fn record_saved_room(n: RoomBuf) {
        SAVED_ROOMS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(n.as_str().to_string());
    }

    #[test]
    fn deciding_the_room_name_setting_asks_to_remember_it_without_an_order() {
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b)
            .with_room(RoomItem::On(RoomBuf::from_name("init-name").unwrap()))
            .with_room_saver(Some(record_saved_room));
        SAVED_ROOMS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let mut g = Quiet {
            screen: 6,
            cursor: 4,
            word: 0,
            im2: InputState::new(),
            fed: [0; 3],
            typed: [0; 8],
            typed_n: 0,
            gate: false,
        };
        let f = facts();
        let put = |g: &mut Quiet, text: &str| {
            for (d, u) in g.typed.iter_mut().zip(text.encode_utf16()) {
                *d = u;
            }
            g.typed_n = text.encode_utf16().count().min(8);
        };
        let saved = || {
            SAVED_ROOMS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        };
        for w in [Z, 0, DOWN, 0, DOWN, 0, Z, 0, 0, 0, 0, 0, 0, 0, 0] {
            g.word = w;
            let _ = ui.tick(&mut g, &f);
        }
        assert!(saved().is_empty(), "決める前に覚える仕事が呼ばれた");
        put(&mut g, "\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}");
        let _ = ui.tick(&mut g, &f);
        put(&mut g, "\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}\r");
        let _ = ui.tick(&mut g, &f);
        assert!(saved().is_empty(), "空の名で覚える仕事が呼ばれた");
        put(&mut g, "room-1\r");
        let _ = ui.tick(&mut g, &f);
        assert_eq!(saved(), vec!["room-1".to_string()]);
        for w in [0, 0, Z, 0, 0, 0, 0, 0, 0, 0, 0, ESC, 0, 0] {
            g.word = w;
            let _ = ui.tick(&mut g, &f);
        }
        assert_eq!(saved().len(), 1, "Esc でやめて覚える仕事が呼ばれた");
        assert_eq!(
            link.take_order(),
            None,
            "覚える指示が指示のリングバッファへ入った"
        );
        assert_eq!(ui.stats().orders_sent, 0);
    }

    #[test]
    fn spectate_screens_render_to_preview_images() {
        use crate::overlay_win::{FONT_FACES, FONT_HEIGHT, FONT_SCALE, gdi_atlas};
        const W: u32 = 640;
        const H: u32 = 480;
        let atlas = gdi_atlas(&FONT_FACES, FONT_HEIGHT, FONT_SCALE).expect("GDI の文字の表");
        let bg: Vec<u8> = (0..W * H)
            .flat_map(|i| {
                let (x, y) = (i % W, i / W);
                [(40 + x * 180 / W) as u8, (60 + y * 120 / H) as u8, 110]
            })
            .collect();
        let out_dir = std::env::var_os("HIGANR_PREVIEW_DIR").map(std::path::PathBuf::from);
        let stamp = format!(
            "{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        let mut shown: Vec<&str> = Vec::new();
        let mut shot = |b: &Board, kind: &'static str| {
            let mut sheet = bg.clone();
            let mut quads = 0usize;
            draw_board(b, &atlas.spec, W, H, &mut |v| {
                quads += blit_quads(
                    v,
                    &atlas.texels,
                    atlas.spec.tex_w,
                    atlas.spec.tex_h,
                    &mut sheet,
                    W,
                    H,
                    0,
                    0,
                );
            });
            assert!(quads > 0 && sheet != bg, "{kind}: 何も描かれていない");
            if let Some(dir) = &out_dir {
                std::fs::create_dir_all(dir).expect("プレビューのフォルダ");
                let path = dir.join(format!("c1_{kind}_{stamp}.png"));
                assert!(!path.exists(), "既存のファイルを上書きしない");
                let big = higanr_core::overlay::upscale_rgb(&sheet, W, H, 2);
                let png = higanr_core::shot::encode_png(W * 2, H * 2, &big).expect("PNG に符号化");
                std::fs::write(&path, png).expect("PNG を書く");
                println!("PNG: {}", path.display());
            }
            shown.push(kind);
        };
        let link = Arc::new(UiLink::new());
        let b = board();
        // ルーム名は未設定で始める（設定済みだと観戦の「ルームマッチ」は入力を経ずに始まって入力の画面が撮れない）。
        let mut ui =
            NetUi::new(ini(), Arc::clone(&link), b).with_room(RoomItem::On(RoomBuf::new()));
        let mut g = Fake::on_match_screen(4);
        let f = facts();
        let note = |seq: u32, state: CodeState, event: Event| Note {
            seq,
            event,
            state,
            room: higanr_core::net_menu::sanitize_room("my-room", |_| true),
            name: higanr_core::net_menu::sanitize_name(b"peer", |_| true),
            since: 1_000_000 - 552,
            ..Note::default()
        };
        g.step(&mut ui, &f, Z);
        g.step(&mut ui, &f, 0);
        assert!(menu_rows(b).iter().any(|l| l == "観戦"));
        shot(b, "1_list");
        // 観戦の「ランダムマッチ」: 一覧（満員の行・8行を越えるスクロール）と空の一覧。
        for _ in 0..3 {
            g.step(&mut ui, &f, DOWN);
            g.step(&mut ui, &f, 0);
        }
        g.step(&mut ui, &f, Z);
        g.step(&mut ui, &f, 0);
        assert!(ui.menu.wants_watch_list());
        let names = [
            "Alice", "Carol", "Player", "Erin", "Grace", "Ivan", "Mallory", "Olivia", "Quinn",
            "Sybil", "Trent", "Victor",
        ];
        let rows: Vec<higanr_core::watch_list::WatchRow> = names
            .iter()
            .enumerate()
            .map(|(i, n)| watch_row(i as u8 + 1, n, "Bob", if i == 2 { 10 } else { i as u8 % 5 }))
            .collect();
        link.publish_watch_list(higanr_core::watch_list::ListStatus::Ready, &rows);
        g.step(&mut ui, &f, 0);
        g.step(&mut ui, &f, 0);
        assert_eq!(menu_rows(b)[0], "観戦 ─ ランダムマッチ");
        shot(b, "1b_watch_list");
        link.publish_watch_list(higanr_core::watch_list::ListStatus::Ready, &[]);
        g.step(&mut ui, &f, 0);
        g.step(&mut ui, &f, 0);
        assert!(
            menu_rows(b)
                .iter()
                .any(|l| l == "  いま観戦できるランダムマッチはありません。")
        );
        shot(b, "1c_watch_empty");
        g.step(&mut ui, &f, X);
        g.step(&mut ui, &f, 0);
        assert!(!ui.menu.wants_watch_list() && ui.menu_active());
        // 観戦のランダムマッチの次は観戦の「ルームマッチ」。
        g.step(&mut ui, &f, DOWN);
        g.step(&mut ui, &f, 0);
        g.step(&mut ui, &f, Z);
        g.step(&mut ui, &f, 0);
        assert_eq!(menu_rows(b)[0], "観戦");
        shot(b, "2_input");
        g.typed.extend("my-room".encode_utf16().chain(Some(0x0D)));
        g.step(&mut ui, &f, 0);
        assert_eq!(link.take_order(), Some(Order::Spectate));
        link.publish(&note(1, CodeState::SpectateWaiting, Event::RoomWaiting));
        g.step(&mut ui, &f, 0);
        g.step(&mut ui, &f, 0);
        assert_eq!(menu_rows(b)[0], "> 観戦をやめる");
        shot(b, "3_menu_waiting");
        g.step(&mut ui, &f, X);
        g.step(&mut ui, &f, 0);
        g.scenes = (2, 0);
        g.step(&mut ui, &f, 0);
        assert_ne!(b.show() & SHOW_WAIT, 0);
        shot(b, "4_band_waiting");
        link.publish(&note(2, CodeState::SpectateLinking, Event::SpectatePeer));
        g.step(&mut ui, &f, 0);
        assert_eq!(row_text(b, ROW_TL0), "相手「peer」に接続しています。");
        shot(b, "5_corner_linking");
        link.publish(&note(
            3,
            CodeState::SpectateIdle,
            Event::SpectateUnresponsive,
        ));
        g.step(&mut ui, &f, 0);
        assert_eq!(row_text(b, ROW_TL0), "サーバーが応答しません。");
        shot(b, "6_corner_failure");
        g.scenes = (SCENE_MENU, 0);
        g.step(&mut ui, &f, Z);
        g.step(&mut ui, &f, 0);
        let rows = menu_rows(b);
        assert_eq!(rows[1], "> ランダムマッチ", "{rows:?}");
        assert_eq!(rows[2], "  ルームマッチ", "{rows:?}");
        assert_eq!(rows[5], "  観戦コード入力", "{rows:?}");
        shot(b, "7_menu_failure");
        assert_eq!(
            shown,
            [
                "1_list",
                "1b_watch_list",
                "1c_watch_empty",
                "2_input",
                "3_menu_waiting",
                "4_band_waiting",
                "5_corner_linking",
                "6_corner_failure",
                "7_menu_failure"
            ]
        );
    }

    #[test]
    fn the_spectate_path_does_not_allocate() {
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b)
            .with_room(RoomItem::On(RoomBuf::from_name("init-name").unwrap()));
        let s = spec();
        let mut g = Quiet {
            screen: 6,
            cursor: 4,
            word: 0,
            im2: InputState::new(),
            fed: [0; 3],
            typed: [0; 8],
            typed_n: 0,
            gate: false,
        };
        let f = facts();
        let put = |g: &mut Quiet, text: &str| {
            for (d, u) in g.typed.iter_mut().zip(text.encode_utf16()) {
                *d = u;
            }
            g.typed_n = text.encode_utf16().count().min(8);
        };
        let note = |seq: u32, state: CodeState, event: Event| Note {
            seq,
            event,
            state,
            room: higanr_core::net_menu::sanitize_room("Watch-room", |_| true),
            name: higanr_core::net_menu::sanitize_name(b"peer", |_| true),
            peer_ver: 7,
            since: 1_000_000 - 61,
            ..Note::default()
        };
        let mut draws = 0usize;
        let mut seq = 10u32;
        for round in 0..2 {
            let before = crate::test_alloc::count();
            // 開く → 「ルーム名」を決定して入力へ → 打ってEnter（決定）→ 下の観戦の「ルームマッチ」を決定する（ルーム名が設定済みなので入力を経ずにその名で始まる）。
            // 前の回の終わりの状態のままだと項目が観戦者のものになるので、何も待っていない状態から始める。
            seq += 1;
            link.publish(&note(seq, CodeState::Idle, Event::None));
            for w in [Z, 0, DOWN, 0, DOWN, 0, Z, 0, 0, 0, 0, 0, 0, 0, 0] {
                g.word = w;
                let _ = ui.tick(&mut g, &f);
            }
            put(&mut g, "ＡＢc-_.あ");
            let _ = ui.tick(&mut g, &f);
            for _ in 0..4 {
                put(&mut g, "\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}");
                let _ = ui.tick(&mut g, &f);
            }
            put(&mut g, "spec-1\r");
            let _ = ui.tick(&mut g, &f);
            for w in [0, DOWN, 0, DOWN, 0, Z, 0] {
                g.word = w;
                let _ = ui.tick(&mut g, &f);
            }
            for st in [
                (CodeState::SpectateJoining, Event::RoomJoining),
                (CodeState::SpectateWaiting, Event::RoomWaiting),
                (CodeState::SpectateLinking, Event::SpectatePeer),
            ] {
                seq += 1;
                link.publish(&note(seq, st.0, st.1));
                for w in [0, 0, 0] {
                    g.word = w;
                    let _ = ui.tick(&mut g, &f);
                }
                draws += draw_board(b, &s, 640, 480, &mut |v| {
                    assert!(v.len() % 6 == 0);
                });
            }
            for w in [Z, 0, 0] {
                g.word = w;
                let _ = ui.tick(&mut g, &f);
            }
            for ev in [
                Event::SpectateUnresponsive,
                Event::SpectateRefused,
                Event::SpectateBadCode,
                Event::SpectateOtherVersion,
                Event::SpectateGaveUp,
                Event::SpectateNoLink,
                Event::SpectateEnded,
                Event::SpectateCancelled,
                Event::SpectateFullEnded,
                Event::SpectateFull,
                Event::SpectateCodeFull,
            ] {
                seq += 1;
                link.publish(&note(seq, CodeState::SpectateIdle, ev));
                for w in [0, 0] {
                    g.word = w;
                    let _ = ui.tick(&mut g, &f);
                }
                draws += draw_board(b, &s, 640, 480, &mut |v| {
                    assert!(v.len() % 6 == 0);
                });
            }
            for w in [X, 0, 0, 0] {
                g.word = w;
                let _ = ui.tick(&mut g, &f);
            }
            draws += draw_board(b, &s, 640, 480, &mut |v| {
                assert!(v.len() % 6 == 0);
            });
            let after = crate::test_alloc::count();
            if round == 1 {
                assert_eq!(after, before, "観戦のルームマッチの経路で確保した");
            }
        }
        assert!(draws >= 20, "盤を描く経路を通っていない（{draws}）");
        let mut orders = Vec::new();
        while let Some(o) = link.take_order() {
            orders.push(o);
        }
        assert_eq!(
            orders,
            [
                Order::Spectate,
                Order::SpectateCancel,
                Order::Spectate,
                Order::SpectateCancel
            ],
            "指示の経路を通っていない"
        );
        assert_eq!(
            link.spectate_name(),
            RoomBuf::from_name("spec-1"),
            "決めた名が観戦の入れ物に届いていない"
        );
        assert_eq!(link.room_name(), None);
    }

    #[test]
    fn the_watch_input_path_does_not_allocate() {
        use crate::netplay::Phase;
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b)
            .with_room(RoomItem::On(RoomBuf::from_name("init-name").unwrap()));
        let (mut wp, mut net) = watch_rig();
        let mut g = Quiet {
            screen: 6,
            cursor: 4,
            word: 0,
            im2: InputState::new(),
            fed: [0; 3],
            typed: [0; 8],
            typed_n: 0,
            gate: false,
        };
        let note = |seq: u32, state: CodeState, event: Event| Note {
            seq,
            event,
            state,
            room: higanr_core::net_menu::sanitize_room("Watch-room", |_| true),
            name: higanr_core::net_menu::sanitize_name(b"peer", |_| true),
            since: 1_000_000 - 61,
            ..Note::default()
        };
        let mut seq = 10u32;
        let _ = watch_input(&mut ui, &mut g, &mut wp, &mut net, 1_000_000);
        for round in 0..2 {
            let before = crate::test_alloc::count();
            wp.set_phase_for_test(Phase::Unconnected);
            g.screen = 6;
            g.cursor = 4;
            for w in [Z, 0, 0] {
                g.word = w;
                let _ = watch_input(&mut ui, &mut g, &mut wp, &mut net, 1_000_000);
            }
            for st in [
                (CodeState::SpectateJoining, Event::RoomJoining),
                (CodeState::SpectateWaiting, Event::RoomWaiting),
                (CodeState::SpectateLinking, Event::SpectatePeer),
            ] {
                seq += 1;
                link.publish(&note(seq, st.0, st.1));
                for w in [0, 0, 0] {
                    g.word = w;
                    let _ = watch_input(&mut ui, &mut g, &mut wp, &mut net, 1_000_000);
                }
            }
            for w in [X, 0, 0, 0] {
                g.word = w;
                let _ = watch_input(&mut ui, &mut g, &mut wp, &mut net, 1_000_000);
            }
            wp.set_phase_for_test(Phase::Selecting);
            g.cursor = 2;
            for _ in 0..40 {
                g.word = 0;
                let _ = watch_input(&mut ui, &mut g, &mut wp, &mut net, 1_000_000);
            }
            g.screen = 7;
            g.cursor = 0;
            for _ in 0..20 {
                g.word = 0;
                let _ = watch_input(&mut ui, &mut g, &mut wp, &mut net, 1_000_000);
            }
            seq += 1;
            link.publish(&note(seq, CodeState::Idle, Event::SpectateEnded));
            for _ in 0..4 {
                g.word = 0;
                let _ = watch_input(&mut ui, &mut g, &mut wp, &mut net, 1_000_000);
            }
            ui.after_watch_teardown();
            let after = crate::test_alloc::count();
            if round == 1 {
                assert_eq!(after, before, "観戦者の入れ物の入力の経路で確保した");
            }
        }
        assert!(
            ui.stats().auto_presses > 0,
            "自動の前進の経路を通っていない"
        );
        assert!(ui.stats().opened > 0, "入口の判定の経路を通っていない");
    }

    #[test]
    fn a_desynced_match_shows_two_warning_lines_stays_frozen_and_esc_clears_them() {
        use crate::netplay::fake::{FRAME_US, FakeGame, FakeNet};
        use crate::netplay::{CheckSource, Phase};
        let (mut na, mut nb, clock) = FakeNet::pair(
            std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            std::net::SocketAddr::from(([127, 0, 0, 1], 2)),
            2_000,
        );
        let cfg = crate::netplay::Config {
            check: CheckSource::Game,
            ..crate::netplay::Config::default()
        };
        let mut np_a = NetPlay::new(pair_link_cfg(2, 0xF000), cfg, &mut na);
        let mut np_b = NetPlay::new(pair_link_cfg(1, 1), cfg, &mut nb);
        let (mut ui_a, _la, board_a) = new_ui();
        let (mut ui_b, _lb, board_b) = new_ui();
        let mk = |seed| {
            let mut g = FakeGame::new(seed, 300, 1, 1);
            g.menu_before_decide = 20;
            g.load_first = 3;
            g
        };
        let (mut ga, mut gb) = (mk(0xA), mk(0xB));
        ga.corrupt_at = Some(40);
        let views = std::cell::Cell::new([None::<bool>; 2]);
        let mut step = |ga: &mut FakeGame, gb: &mut FakeGame| {
            clock.set(clock.get() + FRAME_US);
            let mut acts = [Action::Local; 2];
            ga.begin();
            let (a, _) = net_input(Some(&mut ui_a), ga, &mut np_a, &mut na, 1_000_000);
            ga.after(a, &mut np_a);
            acts[0] = a;
            gb.begin();
            let (b, _) = net_input(Some(&mut ui_b), gb, &mut np_b, &mut nb, 1_000_000);
            gb.after(b, &mut np_b);
            acts[1] = b;
            views.set(
                [&np_a, &np_b].map(|np| crate::replay_save::NetView::of(np).map(|v| v.desynced)),
            );
            (
                acts,
                np_a.is_stopped(),
                np_b.is_stopped(),
                [np_a.phase(), np_b.phase()],
            )
        };
        let mut k = 0;
        loop {
            let (_, sa, sb, _) = step(&mut ga, &mut gb);
            // 自動保存へ見せる値にも止めたマッチの印が立つ（止めたマッチは保存しない）。止める前は立たない。
            assert_eq!(
                views.get()[0].unwrap_or(false),
                sa,
                "A の印が止めた状態と食い違う"
            );
            assert_eq!(
                views.get()[1].unwrap_or(false),
                sb,
                "B の印が止めた状態と食い違う"
            );
            if sa && sb {
                break;
            }
            k += 1;
            assert!(k < 5_000, "止まらない");
        }
        assert_eq!(views.get(), [Some(true), Some(true)]);
        let mut phases = [Phase::Unconnected; 2];
        for _ in 0..5 {
            phases = step(&mut ga, &mut gb).3;
        }
        let want = [
            "同期がずれたため、試合を終了しました。",
            "zipにしたlogsフォルダを添えて、開発者に伝えてください（Escでメニューへ）。",
        ];
        for (who, b) in [("A", board_a), ("B", board_b)] {
            assert_eq!(
                b.show() & (SHOW_TL0 | SHOW_TL1),
                SHOW_TL0 | SHOW_TL1,
                "{who}"
            );
            assert_eq!(row_text(b, ROW_TL0), want[0], "{who}");
            assert_eq!(row_text(b, ROW_TL1), want[1], "{who}");
            for row in [ROW_TL0, ROW_TL1] {
                let mut buf = [0u8; MAX_TEXT_UTF8];
                let style = b.read_row(row, &mut buf).map(|(_, s)| s);
                assert_eq!(style, Some(RowStyle::Warn), "{who}: 警告の色でない");
            }
        }
        assert_eq!(phases, [Phase::Ended, Phase::Ended]);
        let frozen = |g: &FakeGame| (g.rng, g.recs.len(), g.local);
        let (fa, fb) = (frozen(&ga), frozen(&gb));
        let before = crate::test_alloc::count();
        for _ in 0..60 {
            let (acts, _, _, _) = step(&mut ga, &mut gb);
            assert_eq!(acts, [Action::RenderOnly, Action::RenderOnly]);
        }
        let after = crate::test_alloc::count();
        assert_eq!(
            (frozen(&ga), frozen(&gb)),
            (fa, fb),
            "止めた後にゲームが動いた"
        );
        assert_eq!(after, before, "止めている間の入力の経路が確保した");
        ga.esc_down = false;
        step(&mut ga, &mut gb);
        ga.esc_down = true;
        let (acts, _, sb, _) = step(&mut ga, &mut gb);
        assert_eq!(acts, [Action::Local, Action::RenderOnly]);
        assert!(sb);
        step(&mut ga, &mut gb);
        assert_eq!(
            views.get()[0],
            Some(true),
            "Esc で戻った後に止めた試合の印が消えた（最後まで遊んだ試合を保存してしまう）"
        );
        assert_eq!(
            board_a.show() & (SHOW_TL0 | SHOW_TL1),
            0,
            "戻った後も 2 行が残る"
        );
        assert_eq!(
            board_b.show() & (SHOW_TL0 | SHOW_TL1),
            SHOW_TL0 | SHOW_TL1,
            "止めたままの B の 2 行が消えた"
        );
    }

    // ---- 観戦コードのコピー（Ctrl+C） ----

    use crate::digit_keys::{C_BIT as K_C, CTRL_BIT as K_CTRL, MOD_BIT as K_MOD};

    fn linked(host: bool) -> UiFacts {
        UiFacts {
            conn: Conn::Connected {
                seat: Seat::P1,
                host,
                shape: Some((Mode::Rollback, 1)),
                watch_off: false,
            },
            net: NetPhase::Open,
            ..facts()
        }
    }

    /// 本物のキーの読み取りを`keys`にして1 tick回す。
    fn tick_keys(g: &mut Fake, ui: &mut NetUi, f: &UiFacts, keys: Option<u16>) {
        g.keys = keys;
        g.step(ui, f, 0);
    }

    fn orders(link: &UiLink) -> Vec<Order> {
        std::iter::from_fn(|| link.take_order()).collect()
    }

    fn row_style(b: &Board, i: usize) -> RowStyle {
        let mut buf = [0u8; MAX_TEXT_UTF8];
        b.read_row(i, &mut buf).map(|(_, s)| s).unwrap()
    }

    /// 押した瞬間に監視の処理へ1回だけ頼む（押しっぱなしでは繰り返さない）。ホストもゲストも・見つかった間も接続した後も・メニューの場面もマッチの場面も同じ。
    #[test]
    fn ctrl_c_asks_the_watcher_once_per_press_while_linked() {
        for (name, f, scene) in [
            ("ホスト・メニューの場面", linked(true), (SCENE_MENU, 0)),
            ("ホスト・マッチの場面", linked(true), (SCENE_MATCH, 0)),
            ("ゲスト", linked(false), (SCENE_MENU, 0)),
            ("見つかった", found_facts(false), (SCENE_MENU, 0)),
        ] {
            let (mut ui, link, _b) = new_ui();
            let mut g = Fake::on_screen(8, 0);
            g.scenes = scene;
            tick_keys(&mut g, &mut ui, &f, Some(0));
            tick_keys(&mut g, &mut ui, &f, Some(K_CTRL));
            tick_keys(&mut g, &mut ui, &f, Some(0));
            tick_keys(&mut g, &mut ui, &f, Some(K_C));
            tick_keys(&mut g, &mut ui, &f, Some(0));
            assert_eq!(orders(&link), [], "{name}: Ctrl だけ・C だけで頼んだ");
            tick_keys(&mut g, &mut ui, &f, Some(K_C | K_CTRL));
            assert_eq!(orders(&link), [Order::SpectateCopy], "{name}");
            for _ in 0..30 {
                tick_keys(&mut g, &mut ui, &f, Some(K_C | K_CTRL));
            }
            assert_eq!(orders(&link), [], "{name}: 押しっぱなしで繰り返した");
            tick_keys(&mut g, &mut ui, &f, Some(K_CTRL));
            tick_keys(&mut g, &mut ui, &f, Some(K_C | K_CTRL));
            assert_eq!(orders(&link), [Order::SpectateCopy], "{name}: 押し直し");
            assert_eq!(ui.stats().copy_asks, 2, "{name}");
        }
    }

    /// 接続する前はキーを読まない。次のものは取らない: 押したまま接続したキー・読めない間に押されたキー・Alt・Winと一緒のキー・台本の実行。
    #[test]
    fn ctrl_c_is_ignored_before_the_link_after_a_gap_with_alt_and_in_a_scripted_run() {
        let (mut ui, link, _b) = new_ui();
        let mut g = Fake::on_screen(8, 0);
        for _ in 0..5 {
            tick_keys(&mut g, &mut ui, &facts(), Some(K_C | K_CTRL));
        }
        assert_eq!(g.keys_reads, 0, "つながる前にキーを読んだ");
        let f = linked(true);
        tick_keys(&mut g, &mut ui, &f, Some(K_C | K_CTRL));
        assert_eq!(orders(&link), [], "押したままつながって頼んだ");
        tick_keys(&mut g, &mut ui, &f, Some(0));
        tick_keys(&mut g, &mut ui, &f, None);
        tick_keys(&mut g, &mut ui, &f, Some(K_C | K_CTRL));
        assert_eq!(orders(&link), [], "読めない間から押していたキーで頼んだ");
        tick_keys(&mut g, &mut ui, &f, Some(0));
        tick_keys(&mut g, &mut ui, &f, Some(K_C | K_CTRL | K_MOD));
        tick_keys(&mut g, &mut ui, &f, Some(K_C | K_CTRL));
        assert_eq!(orders(&link), [], "Alt・Win と一緒の C で頼んだ");
        tick_keys(&mut g, &mut ui, &f, Some(0));
        tick_keys(&mut g, &mut ui, &f, Some(K_C | K_CTRL));
        assert_eq!(orders(&link), [Order::SpectateCopy]);
        // 台本の実行は本物のキーを読まない。
        let (mut ui, link, _b) = new_ui();
        let mut g = Fake::on_screen(8, 0);
        g.scripted = true;
        for k in [0, K_C | K_CTRL, 0, K_C | K_CTRL] {
            tick_keys(&mut g, &mut ui, &f, Some(k));
        }
        assert_eq!((g.keys_reads, orders(&link)), (0, vec![]));
    }

    #[test]
    fn a_scripted_copy_press_asks_once_and_only_while_linked_in_a_scripted_run() {
        let f = linked(true);
        let (mut ui, link, _b) = new_ui();
        let mut g = Fake::on_screen(8, 0);
        g.scripted = true;
        g.copy_script_next = true;
        tick_keys(&mut g, &mut ui, &f, None);
        assert_eq!(orders(&link), [Order::SpectateCopy]);
        assert_eq!(g.keys_reads, 0, "台本の実行で本物のキーを読んだ");
        tick_keys(&mut g, &mut ui, &f, None);
        assert_eq!(orders(&link), [], "1 回の押下が 2 回の頼みになった");
        // 接続する前は押下を取らない（取り出しは残る）。
        g.copy_script_next = true;
        tick_keys(&mut g, &mut ui, &facts(), None);
        assert_eq!(orders(&link), [], "つながる前に頼んだ");
        assert!(g.copy_script_next, "つながる前に押下を取り出した");
        // 台本の無い実行は`copy_script`を見ない。
        g.scripted = false;
        tick_keys(&mut g, &mut ui, &f, Some(0));
        assert_eq!(orders(&link), [], "台本の無い実行で台本の押下を使った");
        assert_eq!(ui.stats().copy_asks, 1);
    }

    /// 頼んだ後に来た結果だけを左上の知らせの行に数秒間出す。メニューの場面でもマッチの場面でも同じ。
    #[test]
    fn the_copy_result_shows_for_a_few_seconds_after_asking_on_every_scene() {
        for scene in [(SCENE_MENU, 0), (SCENE_MATCH, 0)] {
            for (ev, text, style) in [
                (
                    Event::SpectateCodeCopied,
                    T_SPECTATE_CODE_COPIED.ja,
                    RowStyle::Normal,
                ),
                (
                    Event::SpectateCodeHostOnly,
                    T_SPECTATE_CODE_HOST_ONLY.ja,
                    RowStyle::Normal,
                ),
                (
                    Event::SpectateCodeClipWrite,
                    T_SPECTATE_CODE_COPY_FAILED.ja,
                    RowStyle::Warn,
                ),
            ] {
                let (mut ui, link, b) = new_ui();
                let f = linked(true);
                let mut g = Fake::on_screen(8, 0);
                g.scenes = scene;
                let note = |seq| Note {
                    seq,
                    event: ev,
                    ..Note::default()
                };
                // 頼んでいないのに来ていたイベントは出さない。
                link.publish(&note(5));
                tick_keys(&mut g, &mut ui, &f, Some(0));
                tick_keys(&mut g, &mut ui, &f, Some(0));
                assert_eq!(b.show() & SHOW_COPY, 0, "{ev:?}: 頼んでいないのに出た");
                tick_keys(&mut g, &mut ui, &f, Some(K_C | K_CTRL));
                tick_keys(&mut g, &mut ui, &f, Some(0));
                assert_eq!(b.show() & SHOW_COPY, 0, "{ev:?}: 結果が来る前に出た");
                link.publish(&note(6));
                tick_keys(&mut g, &mut ui, &f, Some(0));
                assert_eq!(b.show() & SHOW_COPY, SHOW_COPY, "{ev:?} {scene:?}");
                assert_eq!(row_text(b, ROW_COPY), text, "{ev:?}");
                assert_eq!(row_style(b, ROW_COPY), style, "{ev:?}");
                let at = |secs| UiFacts {
                    now: f.now + secs,
                    ..f
                };
                tick_keys(&mut g, &mut ui, &at(COPY_NOTICE_SECS - 1), Some(0));
                assert_eq!(b.show() & SHOW_COPY, SHOW_COPY, "{ev:?}: 消えるのが早い");
                tick_keys(&mut g, &mut ui, &at(COPY_NOTICE_SECS), Some(0));
                assert_eq!(b.show() & SHOW_COPY, 0, "{ev:?}: 消えない");
            }
        }
    }

    /// rollbackのマッチの間は`NetUi::tick`が呼ばれない。`copy_tick`だけが毎フレーム呼ばれ、そこでも依頼と結果の知らせが動く。
    #[test]
    fn copy_tick_alone_asks_and_shows_the_result() {
        let (mut ui, link, b) = new_ui();
        let mut g = Fake::on_screen(8, 0);
        g.scenes = (SCENE_MATCH, 0);
        let conn = linked(false).conn;
        let now = 1_000_000;
        g.keys = Some(0);
        ui.copy_tick(&mut g, conn, now);
        g.keys = Some(K_C | K_CTRL);
        ui.copy_tick(&mut g, conn, now);
        assert_eq!(orders(&link), [Order::SpectateCopy]);
        link.publish(&Note {
            seq: 1,
            event: Event::SpectateCodeHostOnly,
            ..Note::default()
        });
        ui.copy_tick(&mut g, conn, now + 1);
        assert_eq!(row_text(b, ROW_COPY), T_SPECTATE_CODE_HOST_ONLY.ja);
        ui.copy_tick(&mut g, Conn::Waiting, now + 2);
        assert_eq!(b.show() & SHOW_COPY, 0, "つながりが終わった後も残った");
    }

    /// 案内が出るのは次の条件がすべて揃ったとき: 対戦のホスト・`[spectate] enable = 1`・「つながりました」の行が出ている間（画面7まで）。
    #[test]
    fn the_hint_shows_only_to_a_host_with_spectating_on_while_the_connected_line_shows() {
        let build = |on: bool| {
            let link = Arc::new(UiLink::new());
            let b = board();
            let ui = NetUi::new(ini(), Arc::clone(&link), b).with_spectate_hint(on);
            (ui, link, b)
        };
        for screen in [6, 7] {
            let (mut ui, _l, b) = build(true);
            let mut g = Fake::on_screen(screen, 0);
            g.step(&mut ui, &linked(true), 0);
            assert_eq!(b.show() & SHOW_COPY, SHOW_COPY, "画面 {screen}");
            assert_eq!(row_text(b, ROW_COPY), T_SPECTATE_CODE_HINT.ja);
            assert_eq!(row_style(b, ROW_COPY), RowStyle::Hint);
            assert_eq!(
                row_text(b, ROW_TL0).chars().next(),
                Some('接'),
                "案内は「つながりました」の行の続き"
            );
        }
        let hidden = |name: &str, on: bool, f: &UiFacts, screen: u32| {
            let (mut ui, _l, b) = build(on);
            let mut g = Fake::on_screen(screen, 0);
            g.step(&mut ui, f, 0);
            assert_eq!(b.show() & SHOW_COPY, 0, "{name}");
        };
        hidden("ゲストに出た", true, &linked(false), 7);
        hidden("観戦を切ったホストに出た", false, &linked(true), 7);
        hidden(
            "つながった行が消える画面 8 に残った",
            true,
            &linked(true),
            8,
        );
        hidden("つながる前に出た", true, &facts(), 7);
        hidden("見つかった間に出た", true, &found_facts(false), 7);
        // 英語。
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b)
            .with_spectate_hint(true)
            .with_lang(Lang::En, None);
        let mut g = Fake::on_screen(7, 0);
        g.step(&mut ui, &linked(true), 0);
        assert_eq!(row_text(b, ROW_COPY), T_SPECTATE_CODE_HINT.en);
        // 結果が出ている間は結果、消えたら案内に戻る。画面8へ進めば案内も消える。
        g.keys = Some(0);
        g.step(&mut ui, &linked(true), 0);
        g.keys = Some(K_C | K_CTRL);
        g.step(&mut ui, &linked(true), 0);
        link.publish(&Note {
            seq: 3,
            event: Event::SpectateCodeCopied,
            ..Note::default()
        });
        g.keys = Some(0);
        g.step(&mut ui, &linked(true), 0);
        assert_eq!(row_text(b, ROW_COPY), T_SPECTATE_CODE_COPIED.en);
        let later = UiFacts {
            now: 1_000_000 + COPY_NOTICE_SECS,
            ..linked(true)
        };
        g.step(&mut ui, &later, 0);
        assert_eq!(row_text(b, ROW_COPY), T_SPECTATE_CODE_HINT.en);
        g.view = Some(MenuView {
            screen: 8,
            sub: 1,
            ..MenuView::default()
        });
        g.step(&mut ui, &later, 0);
        assert_eq!(b.show() & SHOW_COPY, 0);
    }

    #[test]
    fn a_player_whose_opponent_refuses_watching_sees_that_instead_of_the_copy_hint() {
        let off = |host: bool| UiFacts {
            conn: Conn::Connected {
                seat: Seat::P1,
                host,
                shape: Some((Mode::Rollback, 1)),
                watch_off: true,
            },
            net: NetPhase::Open,
            ..facts()
        };
        for host in [true, false] {
            let link = Arc::new(UiLink::new());
            let b = board();
            let mut ui = NetUi::new(ini(), Arc::clone(&link), b).with_spectate_hint(true);
            let mut g = Fake::on_screen(7, 0);
            g.step(&mut ui, &off(host), 0);
            assert_eq!(b.show() & SHOW_COPY, SHOW_COPY, "ホスト {host}");
            assert_eq!(
                row_text(b, ROW_COPY),
                T_SPECTATE_PEER_OFF.ja,
                "ホスト {host}"
            );
            assert_eq!(row_style(b, ROW_COPY), RowStyle::Hint);
            // Ctrl+Cの結果は左上の知らせに出る。
            g.keys = Some(0);
            g.step(&mut ui, &off(host), 0);
            g.keys = Some(K_C | K_CTRL);
            g.step(&mut ui, &off(host), 0);
            link.publish(&Note {
                seq: 5,
                event: Event::SpectateCodePeerOff,
                ..Note::default()
            });
            g.keys = Some(0);
            g.step(&mut ui, &off(host), 0);
            assert_eq!(
                row_text(b, ROW_COPY),
                T_SPECTATE_CODE_PEER_OFF.ja,
                "ホスト {host}"
            );
        }
        // 自分が許可していなければ何も出さない。
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b).with_spectate_hint(false);
        let mut g = Fake::on_screen(7, 0);
        g.step(&mut ui, &off(true), 0);
        assert_eq!(b.show() & SHOW_COPY, 0);
    }

    /// 後始末（再接続）で案内も結果も待ちも消える。知らせの行は盤に描かれる。
    #[test]
    fn the_copy_row_is_cleared_on_teardown_and_drawn_on_the_board() {
        let (mut ui, link, b) = new_ui();
        let mut g = Fake::on_screen(8, 0);
        let f = linked(true);
        tick_keys(&mut g, &mut ui, &f, Some(0));
        tick_keys(&mut g, &mut ui, &f, Some(K_C | K_CTRL));
        link.publish(&Note {
            seq: 1,
            event: Event::SpectateCodeCopied,
            ..Note::default()
        });
        tick_keys(&mut g, &mut ui, &f, Some(0));
        assert_eq!(b.show() & SHOW_COPY, SHOW_COPY);
        let s = spec();
        assert_eq!(
            draw_board(b, &s, 640, 480, &mut |v| assert!(v.len() % 6 == 0)),
            1
        );
        ui.after_teardown(Conn::Finished);
        assert_eq!(b.show() & SHOW_COPY, 0);
        assert_eq!(draw_board(b, &s, 640, 480, &mut |_| {}), 0);
        // 後始末した後に前の結果が来ても頼んでいないので出ない。
        link.publish(&Note {
            seq: 2,
            event: Event::SpectateCodeCopied,
            ..Note::default()
        });
        tick_keys(&mut g, &mut ui, &facts(), Some(0));
        assert_eq!(b.show() & SHOW_COPY, 0);
    }

    // ---- 新しいバージョンの知らせ ----

    thread_local! {
        static ASKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        static FOUND_RAW: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    fn fake_ask() {
        ASKS.with(|a| a.set(a.get() + 1));
    }

    fn fake_found() -> u32 {
        FOUND_RAW.with(std::cell::Cell::get)
    }

    fn asks() -> u32 {
        ASKS.with(std::cell::Cell::get)
    }

    fn news_is_out(f: Found) {
        FOUND_RAW.with(|c| c.set(f.to_raw()));
    }

    fn the_news() -> Found {
        Found::Newer(higanr_core::update_notice::Version::new(1, 2, 3))
    }

    const BUILD: u32 = 7;

    fn update_ui(build: u32) -> (NetUi, &'static Board) {
        let b = board();
        let ui = NetUi::new(ini(), Arc::new(UiLink::new()), b).with_update_check(
            Some(UpdateHooks {
                ask: fake_ask,
                found: fake_found,
            }),
            build,
        );
        (ui, b)
    }

    fn at(f: &UiFacts, secs: u64) -> UiFacts {
        UiFacts {
            now: f.now + secs,
            ..*f
        }
    }

    /// 「妖怪 対 妖怪」の決定でメニューを開く（画面6・カーソル4で決定を押して離す）。
    fn open_the_menu(g: &mut Fake, ui: &mut NetUi, f: &UiFacts) {
        g.step(ui, f, Z);
        g.step(ui, f, 0);
        assert!(ui.menu_active(), "メニューが開かない");
    }

    #[test]
    fn opening_the_menu_asks_but_not_again_within_ten_minutes() {
        let (mut ui, _b) = update_ui(BUILD);
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(asks(), 0, "開く前に頼んだ");
        open_the_menu(&mut g, &mut ui, &facts());
        assert_eq!(asks(), 1);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(asks(), 1, "開いている間に毎 tick 頼んだ");
        // 閉じて10分より前に開き直しても取りに行かない。
        ui.menu.reset();
        open_the_menu(&mut g, &mut ui, &at(&facts(), 599));
        assert_eq!(asks(), 1, "10 分より前の開き直しで頼んだ");
        ui.menu.reset();
        g.step(&mut ui, &at(&facts(), 600), 0);
        open_the_menu(&mut g, &mut ui, &at(&facts(), 600));
        assert_eq!(asks(), 2, "10 分たったのに頼まない");
    }

    #[test]
    fn a_dev_build_or_no_hooks_never_ask_nor_read_the_end_menu() {
        let (mut ui, _b) = update_ui(0);
        let mut g = Fake::on_match_screen(4);
        g.end_menu = true;
        open_the_menu(&mut g, &mut ui, &facts());
        for f in [linked(true), at(&linked(true), 100_000)] {
            g.step(&mut ui, &f, 0);
        }
        assert_eq!((asks(), g.end_reads), (0, 0));
        let b = board();
        let mut bare = NetUi::new(ini(), Arc::new(UiLink::new()), b);
        let mut g2 = Fake::on_match_screen(4);
        open_the_menu(&mut g2, &mut bare, &facts());
        g2.step(&mut bare, &linked(true), 0);
        assert_eq!((asks(), g2.end_reads), (0, 0), "設定が無いのに動いた");
    }

    #[test]
    fn a_connection_asks_once_and_the_end_menu_waits_thirty_minutes() {
        let (mut ui, _b) = update_ui(BUILD);
        let mut g = Fake::on_screen(8, 0);
        g.scenes = (SCENE_MATCH, 0);
        let f = linked(true);
        g.step(&mut ui, &facts(), 0);
        g.step(&mut ui, &f, 0);
        assert_eq!(asks(), 1, "接続が成立したときに頼まない");
        g.step(&mut ui, &f, 0);
        assert_eq!(asks(), 1);
        // 終わりのメニューは前の確認から30分たってから。
        g.end_menu = true;
        g.step(&mut ui, &at(&f, 1799), 0);
        assert_eq!(asks(), 1, "30 分より前に頼んだ");
        g.end_menu = false;
        g.step(&mut ui, &at(&f, 1800), 0);
        g.end_menu = true;
        g.step(&mut ui, &at(&f, 1800), 0);
        assert_eq!(asks(), 2, "30 分たったのに頼まない");
        g.step(&mut ui, &at(&f, 1801), 0);
        assert_eq!(asks(), 2, "出ている間に毎 tick 頼んだ");
    }

    #[test]
    fn the_end_menu_does_nothing_without_a_connection() {
        let (mut ui, b) = update_ui(BUILD);
        news_is_out(the_news());
        let mut g = Fake::on_screen(8, 0);
        g.scenes = (SCENE_MATCH, 0);
        g.end_menu = true;
        for f in [facts(), found_facts(false)] {
            g.step(&mut ui, &f, 0);
            assert_eq!(b.show() & SHOW_UPDATE, 0, "接続していないのに出た");
        }
        assert_eq!(g.end_reads, 0, "接続していないのに読んだ");
    }

    #[test]
    fn once_found_it_stops_asking_and_shows_on_the_end_menu_only_when_connected() {
        let (mut ui, b) = update_ui(BUILD);
        let mut g = Fake::on_screen(8, 0);
        g.scenes = (SCENE_MATCH, 0);
        let f = linked(true);
        g.step(&mut ui, &f, 0);
        assert_eq!(asks(), 1);
        news_is_out(the_news());
        // 試合中（終わりのメニューが出ていない）は出さない。
        g.step(&mut ui, &f, 0);
        assert_eq!(b.show() & SHOW_UPDATE, 0, "試合中に出た");
        g.end_menu = true;
        g.step(&mut ui, &f, 0);
        assert_eq!(b.show() & SHOW_UPDATE, SHOW_UPDATE);
        assert_eq!(
            row_text(b, ROW_UPD0),
            "新しいバージョンがリリースされています（v1.2.3）"
        );
        assert_eq!(
            row_text(b, ROW_UPD1),
            "更新する場合は開いている花映塚を閉じ、HiganRTool.exeから起動しなおしてください。"
        );
        assert_eq!(row_style(b, ROW_UPD0), RowStyle::Notice);
        assert_eq!(row_style(b, ROW_UPD1), RowStyle::Notice);
        // 見つかった後は時間がたっても・立ち上がりがあっても取りに行かない。
        for k in 1..4u64 {
            g.end_menu = false;
            g.step(&mut ui, &at(&f, k * 100_000), 0);
            g.end_menu = true;
            g.step(&mut ui, &at(&f, k * 100_000), 0);
        }
        assert_eq!(asks(), 1, "見つかった後に頼んだ");
        assert_eq!(b.show() & SHOW_UPDATE, SHOW_UPDATE);
        // 終わりのメニューが消えれば、左上の行も消える。
        g.end_menu = false;
        g.step(&mut ui, &f, 0);
        assert_eq!(
            b.show() & SHOW_UPDATE,
            0,
            "終わりのメニューが消えても残った"
        );
        // 接続が終われば、終わりのメニューが出ていても出さない。
        g.end_menu = true;
        g.step(&mut ui, &f, 0);
        assert_eq!(b.show() & SHOW_UPDATE, SHOW_UPDATE);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(b.show() & SHOW_UPDATE, 0, "接続が終わった後も残った");
    }

    #[test]
    fn the_notice_is_in_english_and_without_a_version_when_the_display_is_unreadable() {
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::new(UiLink::new()), b)
            .with_lang(Lang::En, None)
            .with_update_check(
                Some(UpdateHooks {
                    ask: fake_ask,
                    found: fake_found,
                }),
                BUILD,
            );
        news_is_out(Found::Newer(None));
        let mut g = Fake::on_screen(8, 0);
        g.scenes = (SCENE_MATCH, 0);
        g.end_menu = true;
        g.step(&mut ui, &linked(true), 0);
        assert_eq!(row_text(b, ROW_UPD0), "A new version has been released");
        assert_eq!(
            row_text(b, ROW_UPD1),
            "To update, close the game and restart it from HiganRTool.exe."
        );
    }

    #[test]
    fn the_notice_goes_under_the_menu_while_it_is_open_and_not_in_the_corner() {
        let (mut ui, b) = update_ui(BUILD);
        news_is_out(the_news());
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), 0);
        open_the_menu(&mut g, &mut ui, &facts());
        g.step(&mut ui, &facts(), 0);
        let rows = menu_rows(b);
        let n = rows.len();
        assert_eq!(
            rows[n - 2],
            "新しいバージョンがリリースされています（v1.2.3）"
        );
        assert_eq!(
            rows[n - 1],
            "更新する場合は開いている花映塚を閉じ、HiganRTool.exeから起動しなおしてください。"
        );
        assert_eq!(row_style(b, n - 1), RowStyle::Notice);
        assert_eq!(b.show() & SHOW_UPDATE, 0, "メニューの間に左上へも出た");
        // 接続していて終わりのメニューが出ていても、メニューが開いている間は左上に出さない。
        let f = linked(true);
        g.end_menu = true;
        g.step(&mut ui, &f, 0);
        assert_eq!(b.show() & SHOW_UPDATE, 0);
    }

    #[test]
    fn the_notice_rows_are_drawn_after_the_other_corner_rows() {
        let (mut ui, b) = update_ui(BUILD);
        news_is_out(the_news());
        let mut g = Fake::on_screen(8, 0);
        g.scenes = (SCENE_MATCH, 0);
        g.end_menu = true;
        g.step(&mut ui, &linked(true), 0);
        let notice_bits = b.show() & SHOW_UPDATE;
        assert_eq!(notice_bits, SHOW_UPDATE);
        let s = spec();
        // 描いたまとまりごとの (左端, 右端, 上端, 下端)。
        let extents = |show: u32| {
            b.set_show(show);
            let mut out = Vec::new();
            draw_board(b, &s, 640, 480, &mut |v| {
                assert!(v.len() % 6 == 0);
                let f = |pick: fn(&Vertex) -> f32, init: f32, step: fn(f32, f32) -> f32| {
                    v.iter().map(pick).fold(init, step)
                };
                out.push((
                    f(|p| p.x, f32::MAX, f32::min),
                    f(|p| p.x, f32::MIN, f32::max),
                    f(|p| p.y, f32::MAX, f32::min),
                    f(|p| p.y, f32::MIN, f32::max),
                ));
            });
            out
        };
        let only = extents(notice_bits);
        assert_eq!(only.len(), 2, "知らせだけなら 2 行ぶんのまとまり");
        assert!(only[0].2 < only[1].2, "{only:?}");
        // 左上の行の置き場（上の枠の下）から始まる。
        assert!(
            only[0].2 >= (TL_Y as f32) - 1.0 - lay::PAD as f32,
            "{only:?}"
        );

        // 観戦コードの知らせ（左上の続き）と名前の帯を同時に出す。知らせの行はそれらより下。
        b.set_row(ROW_COPY, "copy", RowStyle::Hint);
        b.set_row(ROW_NAME_L, "1P ab", RowStyle::Normal);
        b.set_row(ROW_NAME_R, "cd 2P", RowStyle::Normal);
        let copy_alone = extents(SHOW_COPY);
        assert_eq!(copy_alone.len(), 1);
        let all = extents(notice_bits | SHOW_COPY | SHOW_NAMES);
        assert_eq!(
            all.len(),
            5,
            "観戦コードの知らせ 1 ＋ 知らせ 2 ＋ 名前の帯 2"
        );
        let width = |e: &(f32, f32, f32, f32)| e.1 - e.0;
        let copy: Vec<_> = all
            .iter()
            .filter(|e| width(e) == width(&copy_alone[0]))
            .collect();
        assert_eq!(
            copy.len(),
            1,
            "観戦コードの知らせのまとまりが見つからない: {all:?}"
        );
        let bands: Vec<_> = all.iter().filter(|e| e.2 < 1.0).collect();
        assert_eq!(bands.len(), 2, "名前の帯は上の枠の高さ: {all:?}");
        let bands_bottom = bands.iter().map(|e| e.3).fold(f32::MIN, f32::max);
        let notice: Vec<_> = all
            .iter()
            .filter(|e| e.2 >= 1.0 && width(e) != width(&copy_alone[0]))
            .collect();
        assert_eq!(notice.len(), 2, "{all:?}");
        for n in &notice {
            assert!(n.2 > copy[0].2, "知らせが観戦コードの知らせより上: {all:?}");
            assert!(n.2 >= bands_bottom, "知らせが名前の帯と重なる: {all:?}");
        }
    }

    // ---- プレイヤー名 ----

    /// 名前を覚える仕事の記録（本物は別のスレッドがiniへ書く`ini_save::save_name`）。
    static SAVED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    fn record_saved(n: NameBuf) {
        SAVED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(n.as_str().to_string());
    }

    static LAN_SAVED: std::sync::Mutex<Vec<LanAddr>> = std::sync::Mutex::new(Vec::new());

    fn record_lan(a: LanAddr) {
        LAN_SAVED.lock().unwrap_or_else(|e| e.into_inner()).push(a);
    }

    #[test]
    fn the_lan_items_ask_the_caller_directly_and_remember_the_typed_address() {
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b)
            .with_lan_addr(AddrBuf::from_ini(Some("10.0.0.5")), Some(record_lan));
        LAN_SAVED.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        // ホスト: 入力なしで呼び手へ。
        cursor_to(&mut g, &mut ui, b, "ホスト");
        let o = g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(o.lan, Some(LanAsk::Host));
        assert!(
            !o.open_net && !o.spectate_open,
            "LAN は自分でネットワークを開く"
        );
        assert_eq!(link.take_order(), None, "見張りへの指示にしてはいけない");
        assert_eq!(ui.stats().orders_sent, 0);
        // クライアント: 初めの値は前回のアドレス。打ち足して決定すると呼び手へ届いてiniへ覚えさせる。
        cursor_to(&mut g, &mut ui, b, "クライアント");
        let o = g.step(&mut ui, &facts(), Z);
        assert_eq!(o.lan, None, "入力を経ずに頼みが出た");
        g.step(&mut ui, &facts(), 0);
        assert_eq!(menu_rows(b)[0], "LAN ─ クライアント");
        assert_eq!(menu_rows(b)[2], "ホストのアドレス: 10.0.0.5|");
        g.typed.extend(":19000".encode_utf16().chain(Some(0x0D)));
        let o = g.step(&mut ui, &facts(), 0);
        let want = LanAddr {
            ip: std::net::Ipv4Addr::new(10, 0, 0, 5),
            port: 19000,
        };
        assert_eq!(o.lan, Some(LanAsk::Client(want)));
        assert_eq!(*LAN_SAVED.lock().unwrap(), vec![want]);
        assert_eq!(link.take_order(), None);
        // 観戦: 次の初めの値は今決定したアドレス。
        g.step(&mut ui, &facts(), 0);
        g.step(&mut ui, &facts(), 0);
        cursor_to(&mut g, &mut ui, b, "観戦");
        // 入口の「観戦」の見出しの下の行とは別にLANの中の「観戦」を選ぶ。
        let rows = menu_rows(b);
        let lan_at = rows.iter().position(|r| r == "LAN").unwrap();
        while !menu_rows(b).iter().skip(lan_at).any(|r| r == "> 観戦") {
            g.step(&mut ui, &facts(), DOWN);
            g.step(&mut ui, &facts(), 0);
        }
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(menu_rows(b)[0], "LAN ─ 観戦");
        assert_eq!(menu_rows(b)[2], "ホストのアドレス: 10.0.0.5:19000|");
        // 「:19000」の6文字を消して、ポートを省いた形にする。
        g.typed
            .extend("\u{8}".repeat(6).encode_utf16().chain(Some(0x0D)));
        let o = g.step(&mut ui, &facts(), 0);
        assert_eq!(
            o.lan,
            Some(LanAsk::Watch(LanAddr {
                ip: std::net::Ipv4Addr::new(10, 0, 0, 5),
                port: 17723
            }))
        );
    }

    static VPN_SCANS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn count_vpn_scan() {
        VPN_SCANS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn radmin_set() -> VpnSet {
        VpnSet::from_nets([higanr_core::vpn_net::Net4::new(
            std::net::Ipv4Addr::new(26, 0, 0, 1),
            8,
        )
        .unwrap()])
    }

    #[test]
    fn the_lan_address_input_asks_for_the_vpn_networks_once_per_entry_and_takes_a_vpn_address() {
        use std::sync::atomic::Ordering;
        let enter_client = |ui: &mut NetUi, g: &mut Fake, b| {
            cursor_to(g, ui, b, "クライアント");
            g.step(ui, &facts(), Z);
            g.step(ui, &facts(), 0);
            assert_eq!(menu_rows(b)[0], "LAN ─ クライアント");
        };
        let typed_vpn = |g: &mut Fake| {
            g.typed
                .extend("26.12.34.56".encode_utf16().chain(Some(0x0D)));
        };
        let hooks = VpnHooks {
            scan: count_vpn_scan,
            read: radmin_set,
        };
        // 仕事あり。
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::new(UiLink::new()), b).with_vpn_hooks(Some(hooks));
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        let before = VPN_SCANS.load(Ordering::SeqCst);
        enter_client(&mut ui, &mut g, b);
        for _ in 0..5 {
            g.step(&mut ui, &facts(), 0);
        }
        assert_eq!(
            VPN_SCANS.load(Ordering::SeqCst) - before,
            1,
            "入った 1 回だけ頼む"
        );
        typed_vpn(&mut g);
        let o = g.step(&mut ui, &facts(), 0);
        assert_eq!(
            o.lan,
            Some(LanAsk::Client(LanAddr {
                ip: std::net::Ipv4Addr::new(26, 12, 34, 56),
                port: 17723
            }))
        );
        // 出て入り直す: もう1回頼む。
        for _ in 0..3 {
            g.step(&mut ui, &facts(), 0);
        }
        enter_client(&mut ui, &mut g, b);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(VPN_SCANS.load(Ordering::SeqCst) - before, 2);
        // 仕事なし: 同じアドレスを断って理由を出す。
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::new(UiLink::new()), b);
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        enter_client(&mut ui, &mut g, b);
        typed_vpn(&mut g);
        let o = g.step(&mut ui, &facts(), 0);
        assert_eq!(o.lan, None);
        assert!(
            menu_rows(b)
                .iter()
                .any(|r| r == higanr_core::net_menu::T_LAN_ADDR_NOT_LAN.ja),
            "{:?}",
            menu_rows(b)
        );
    }

    #[test]
    fn deciding_a_player_name_reaches_the_link_and_asks_to_remember_it() {
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b)
            .with_player_name(NameBuf::lossy("Alice-synth"), Some(record_saved));
        SAVED.lock().unwrap_or_else(|e| e.into_inner()).clear();
        assert_eq!(link.player_name(), None, "決める前から名前が読める");
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        let rows = menu_rows(b);
        assert!(
            rows.contains(&"  設定\u{3000}Alice-synth".to_string()),
            "入口のメニューの「設定」の行に今の名前が出ない: {rows:?}"
        );
        open_settings(&mut g, &mut ui, b);
        let rows = menu_rows(b);
        assert_eq!(rows[0], "設定", "{rows:?}");
        assert_eq!(rows[1], "> プレイヤー名\u{3000}Alice-synth", "{rows:?}");
        let o = g.step(&mut ui, &facts(), Z);
        assert!(!o.open_net, "名前の入力がネットワークを開く頼みになった");
        assert_eq!(link.player_name(), None, "入力を経ずに名前が決まった");
        assert_eq!(menu_rows(b)[2], "プレイヤー名: Alice-synth|");
        assert_eq!(g.gate_calls, vec![true], "入力に入ったのにゲートを開けない");
        g.step(&mut ui, &facts(), 0);
        g.step(&mut ui, &facts(), ESC);
        g.step(&mut ui, &facts(), 0);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(link.player_name(), None);
        assert!(SAVED.lock().unwrap().is_empty(), "やめたのに覚えた");
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        g.typed.extend(
            std::iter::repeat_n(0x08u16, 11)
                .chain("Bob_synth_".encode_utf16())
                .chain(Some(0x0D)),
        );
        let mut opened = false;
        for _ in 0..2 {
            opened |= g.step(&mut ui, &facts(), 0).open_net;
        }
        assert!(!opened);
        assert_eq!(
            link.take_order(),
            None,
            "名前が指示のリングバッファへ入った"
        );
        assert_eq!(link.player_name(), NameBuf::from_name("Bob_synth_"));
        assert_eq!(
            *SAVED.lock().unwrap(),
            vec!["Bob_synth_".to_string()],
            "覚える仕事が決めた名前で 1 回だけ呼ばれない"
        );
        assert_eq!(ui.stats().orders_sent, 0, "名前は指示に数えない");
        assert_eq!(
            g.gate_calls,
            vec![true, false, true, false],
            "やめた・決めたときにゲートを閉じない"
        );
        g.step(&mut ui, &facts(), 0);
        assert_eq!(
            menu_rows(b)[1],
            "> プレイヤー名\u{3000}Bob_synth_",
            "決めた後は設定の画面に戻る: {:?}",
            menu_rows(b)
        );
        let (mut ui2, _l, b2) = new_ui();
        let mut g2 = Fake::on_match_screen(4);
        g2.step(&mut ui2, &facts(), Z);
        assert!(!menu_rows(b2).iter().any(|l| l.contains("プレイヤー名")));
    }

    // ---- 画面の言語 ----

    /// 言語をiniへ書く仕事の記録（本物は`script_win::save_ui_lang`）。
    static LANG_SAVED: std::sync::Mutex<Vec<Lang>> = std::sync::Mutex::new(Vec::new());

    fn record_lang(l: Lang) {
        LANG_SAVED.lock().unwrap_or_else(|e| e.into_inner()).push(l);
    }

    #[test]
    fn the_language_row_redraws_the_menu_and_asks_to_remember_it_once_per_change() {
        const LEFT: u16 = 0x0040;
        const RIGHT: u16 = 0x0080;
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b).with_lang(Lang::Ja, Some(record_lang));
        LANG_SAVED.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        open_settings(&mut g, &mut ui, b);
        let rows = menu_rows(b);
        assert!(
            rows.contains(&"  言語\u{3000}\u{3000}\u{3000}◀ 日本語 ▶".to_string()),
            "{rows:?}"
        );
        for _ in 0..2 {
            g.step(&mut ui, &facts(), DOWN);
            g.step(&mut ui, &facts(), 0);
        }
        g.step(&mut ui, &facts(), RIGHT);
        let rows = menu_rows(b);
        assert!(
            rows.contains(&"> Language  ◀ English ▶".to_string()),
            "{rows:?}"
        );
        assert_eq!(
            rows.last().map(String::as_str),
            Some(higanr_core::net_menu::T_KEYS.en),
            "その tick から英語"
        );
        g.step(&mut ui, &facts(), 0);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(
            *LANG_SAVED.lock().unwrap(),
            vec![Lang::En],
            "覚える仕事が替えた回に 1 回だけ呼ばれない"
        );
        assert_eq!(ui.stats().orders_sent, 0, "言語は指示に数えない");
        assert_eq!(
            link.take_order(),
            None,
            "言語が指示のリングバッファへ入った"
        );
        g.step(&mut ui, &facts(), LEFT);
        assert!(menu_rows(b).contains(&"> 言語\u{3000}\u{3000}\u{3000}◀ 日本語 ▶".to_string()));
        assert_eq!(*LANG_SAVED.lock().unwrap(), vec![Lang::En, Lang::Ja]);
    }

    // ---- 同期方式・ディレイ・サイド ----

    static SHAPE_SAVED: std::sync::Mutex<Vec<(Pick, DelayChoice)>> =
        std::sync::Mutex::new(Vec::new());
    static SEAT_SAVED: std::sync::Mutex<Vec<SeatWish>> = std::sync::Mutex::new(Vec::new());

    fn record_shape(p: Pick, d: DelayChoice) {
        SHAPE_SAVED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((p, d));
    }

    fn record_seat(w: SeatWish) {
        SEAT_SAVED.lock().unwrap_or_else(|e| e.into_inner()).push(w);
    }

    /// 開いたメニューから入口のメニューの「設定」の行へ動かして決定し、設定の画面を開く。
    fn open_settings(g: &mut Fake, ui: &mut NetUi, b: &'static Board) {
        cursor_to(g, ui, b, "設定");
        g.step(ui, &facts(), Z);
        g.step(ui, &facts(), 0);
    }

    /// カーソルを`label`の行まで下げる（設定の画面の行は 名前 → サイド → 同期方式 → ディレイ → 言語 の順）。
    fn cursor_to(g: &mut Fake, ui: &mut NetUi, b: &'static Board, label: &str) {
        for _ in 0..16 {
            if menu_rows(b)
                .iter()
                .any(|r| r.starts_with(&format!("> {label}")))
            {
                return;
            }
            g.step(ui, &facts(), DOWN);
            g.step(ui, &facts(), 0);
        }
        panic!("{label} の行に着かない: {:?}", menu_rows(b));
    }

    #[test]
    fn changing_the_side_the_netcode_or_the_delay_asks_to_write_the_ini_once_per_change() {
        const LEFT: u16 = 0x0040;
        const RIGHT: u16 = 0x0080;
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b)
            .with_seat(SeatWish::Any)
            .with_shape_saver(Some(record_shape))
            .with_seat_saver(Some(record_seat));
        SHAPE_SAVED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        SEAT_SAVED.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        open_settings(&mut g, &mut ui, b);
        cursor_to(&mut g, &mut ui, b, "サイド");
        g.step(&mut ui, &facts(), LEFT);
        g.step(&mut ui, &facts(), 0);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(*SEAT_SAVED.lock().unwrap(), vec![SeatWish::P2]);
        g.step(&mut ui, &facts(), RIGHT);
        g.step(&mut ui, &facts(), 0);
        assert_eq!(
            *SEAT_SAVED.lock().unwrap(),
            vec![SeatWish::P2, SeatWish::Any]
        );
        assert!(SHAPE_SAVED.lock().unwrap().is_empty(), "サイドで形を頼んだ");
        cursor_to(&mut g, &mut ui, b, "同期方式");
        g.step(&mut ui, &facts(), RIGHT);
        g.step(&mut ui, &facts(), 0);
        g.step(&mut ui, &facts(), 0);
        {
            let saved = SHAPE_SAVED.lock().unwrap();
            assert_eq!(saved.len(), 1, "替えた回に 1 回だけ頼まない: {saved:?}");
            assert_eq!(saved[0].0, Pick::Delay);
        }
        cursor_to(&mut g, &mut ui, b, "ディレイ");
        g.step(&mut ui, &facts(), RIGHT);
        g.step(&mut ui, &facts(), 0);
        {
            let saved = SHAPE_SAVED.lock().unwrap();
            assert_eq!(saved.len(), 2, "{saved:?}");
            assert_eq!(saved[1].0, Pick::Delay);
            assert!(
                matches!(saved[1].1, DelayChoice::Fixed(_)),
                "ディレイが固定の値にならない: {saved:?}"
            );
            assert_ne!(saved[1].1, saved[0].1, "ディレイを替えたのに値が同じ");
        }
        g.step(&mut ui, &facts(), 0);
        assert_eq!(
            SHAPE_SAVED.lock().unwrap().len(),
            2,
            "替えない tick で頼んだ"
        );
        assert_eq!(SEAT_SAVED.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_ui_started_in_english_draws_the_corner_and_the_match_rows_in_english() {
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), link, b).with_lang(Lang::En, None);
        let mut g = Fake::on_screen(1, 0);
        g.scenes = (2, 0);
        let unmatched = UiFacts {
            conn: Conn::Unmatched(NoMatch::PeerLeft),
            net: NetPhase::Open,
            ..facts()
        };
        g.step(&mut ui, &unmatched, 0);
        assert_eq!(
            row_text(b, ROW_TL0),
            "The opponent closed the connection before starting."
        );
        assert_eq!(row_text(b, ROW_TL1), T_AGAIN.en);
        ui.hud(
            true,
            HudFacts {
                format: Some(Mode::Delay),
                delay_only: true,
                ..hud_with(None)
            },
        );
        assert_eq!(row_text(b, ROW_HUD), "delay only, Delay 1F (0F)  ping 33ms");
        let (mut ja, _l, bj) = new_ui();
        ja.hud(
            true,
            HudFacts {
                format: Some(Mode::Delay),
                delay_only: true,
                ..hud_with(None)
            },
        );
        assert_eq!(
            row_text(bj, ROW_HUD),
            "delay only・Delay 1F (0F)  ping 33ms"
        );
    }

    fn names_of(seat: Seat) -> HudNames {
        HudNames::of(seat, Name::new("synth-own"), Name::new("synth-peer"))
    }

    fn hud_with(names: Option<HudNames>) -> HudFacts {
        HudFacts {
            latency: Some(1),
            rtt_ms: Some(33),
            rewound: None,
            recommend: Some(0),
            names,
            ..HudFacts::default()
        }
    }

    #[test]
    fn the_name_bands_follow_the_seats_and_go_away_with_the_match() {
        let (mut ui, _l, b) = new_ui();
        assert_eq!(b.show() & SHOW_NAMES, 0, "対戦の前から名前が出ている");
        ui.hud(true, hud_with(Some(names_of(Seat::P1))));
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), SHOW_HUD | SHOW_NAMES);
        assert_eq!(row_text(b, ROW_NAME_L), "1P synth-own");
        assert_eq!(row_text(b, ROW_NAME_R), "synth-peer 2P");
        ui.hud(true, hud_with(Some(names_of(Seat::P2))));
        assert_eq!(row_text(b, ROW_NAME_L), "1P synth-peer");
        assert_eq!(row_text(b, ROW_NAME_R), "synth-own 2P");
        ui.publish_corner(&facts());
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), SHOW_HUD | SHOW_NAMES);
        ui.publish_menu(&facts());
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), SHOW_HUD | SHOW_NAMES);
        ui.hud(false, HudFacts::default());
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), 0);
        ui.hud(true, hud_with(Some(names_of(Seat::P2))));
        assert_eq!(b.show() & SHOW_NAMES, SHOW_NAMES);
        assert_eq!(row_text(b, ROW_NAME_L), "1P synth-peer");
        ui.hud(true, hud_with(None));
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), SHOW_HUD);
        let weird = HudNames {
            p1: Name::new("a\nb"),
            p2: Name::new(&"W".repeat(40)),
        };
        ui.hud(true, hud_with(Some(weird)));
        assert_eq!(row_text(b, ROW_NAME_L), "1P a?b");
        assert!(row_text(b, ROW_NAME_R).ends_with(" 2P"));
    }

    #[test]
    fn the_next_select_notice_is_shown_while_waiting_and_cleared_by_the_watch_teardown() {
        let (mut ui, _l, b) = new_ui();
        ui.watch_select_wait(true);
        assert_eq!(b.show() & SHOW_PEER_WAIT, SHOW_PEER_WAIT);
        assert_eq!(row_text(b, ROW_PEER_WAIT), T_SPECTATE_NEXT_SELECT.ja);
        ui.watch_select_wait(false);
        assert_eq!(b.show() & SHOW_PEER_WAIT, 0, "待ちが解けても残った");
        ui.watch_select_wait(true);
        ui.after_watch_teardown();
        assert_eq!(b.show() & SHOW_PEER_WAIT, 0, "観戦を手放した後も残った");
    }

    #[test]
    fn the_watch_notice_switches_its_text_and_clears() {
        let (mut ui, _l, b) = new_ui();
        for (n, want) in [
            (WatchNotice::Paused, T_SPECTATE_PAUSED.ja),
            (WatchNotice::EscHold, T_SPECTATE_ESC_HOLD.ja),
            (WatchNotice::NextSelect, T_SPECTATE_NEXT_SELECT.ja),
        ] {
            ui.watch_notice(Some(n));
            assert_eq!(b.show() & SHOW_PEER_WAIT, SHOW_PEER_WAIT, "{n:?}");
            assert_eq!(row_text(b, ROW_PEER_WAIT), want, "{n:?}: 文が替わらない");
        }
        ui.watch_notice(None);
        assert_eq!(b.show() & SHOW_PEER_WAIT, 0, "消えない");
        ui.watch_notice(Some(WatchNotice::Paused));
        assert_eq!(row_text(b, ROW_PEER_WAIT), T_SPECTATE_PAUSED.ja);
        ui.after_watch_teardown();
        assert_eq!(b.show() & SHOW_PEER_WAIT, 0, "観戦を手放した後も残った");
    }

    #[test]
    fn the_peer_wait_line_survives_the_other_updates_and_is_drawn_under_the_corner_lines() {
        let (mut ui, _l, b) = new_ui();
        ui.peer_wait(false);
        assert_eq!(b.show() & SHOW_PEER_WAIT, 0);
        ui.hud(true, hud_with(Some(names_of(Seat::P1))));
        ui.peer_wait(true);
        assert_eq!(row_text(b, ROW_PEER_WAIT), T_PEER_WAIT.ja);
        // マッチの場面のtick・左上の行・メニューの更新・マッチ中の行を消す更新のどれも知らせを消さない。
        let mut g = Fake::on_screen(6, 0);
        g.scenes = (SCENE_MATCH, 0);
        g.view = None;
        g.step(&mut ui, &connected_facts(), 0);
        ui.publish_corner(&facts());
        ui.publish_menu(&facts());
        ui.hud(false, HudFacts::default());
        assert_eq!(
            b.show() & SHOW_PEER_WAIT,
            SHOW_PEER_WAIT,
            "ほかの更新で消えた"
        );
        assert_eq!(b.show() & SHOW_MENU, SHOW_MENU, "メニューの旗の確かめ");
        ui.peer_wait(true);
        ui.peer_wait(false);
        assert_eq!(b.show() & SHOW_PEER_WAIT, 0, "待ちが解けても残った");
        ui.peer_wait(true);
        ui.after_teardown(Conn::Finished);
        assert_eq!(b.show() & SHOW_PEER_WAIT, 0, "片づけた後も残った");

        // 描く: 左上の行の続き（左上の行が無ければ1行目）。
        let s = spec();
        let (sw, sh) = (640u32, 480u32);
        let texels = vec![0xFFFF_FFFFu32; s.texel_count()];
        let row_h = lay::row_height(&s);
        let b = board();
        b.set_row(ROW_TL0, "abc", RowStyle::Normal);
        b.set_row(ROW_PEER_WAIT, "xyz", RowStyle::Normal);
        let lit = |show: u32| {
            b.set_show(show);
            let mut canvas = vec![0u8; (sw * sh * 3) as usize];
            draw_board(b, &s, sw, sh, &mut |v| {
                blit_quads(v, &texels, s.tex_w, s.tex_h, &mut canvas, sw, sh, 0, 0);
            });
            let px = |y: u32| canvas[((y * sw + TL_X) * 3) as usize];
            (px(TL_Y), px(TL_Y + row_h))
        };
        assert_eq!(
            lit(SHOW_PEER_WAIT).0,
            255,
            "左上の行が無いのに 1 行目に描かない"
        );
        assert_eq!(lit(SHOW_PEER_WAIT).1, 0);
        assert_eq!(
            lit(SHOW_TL0 | SHOW_PEER_WAIT),
            (255, 255),
            "左上の行の続きに描かない"
        );
        assert_eq!(lit(SHOW_TL0), (255, 0));
    }

    #[test]
    fn the_watchers_name_bands_and_status_line_are_the_players_look_and_go_away() {
        let (mut ui, _l, b) = new_ui();
        let facts = |names, delay_frames, fast_forward| WatchHudFacts {
            names,
            stats: None,
            delay_frames,
            fast_forward,
        };
        let row_style = |b: &Board, i: usize| {
            let mut buf = [0u8; MAX_TEXT_UTF8];
            b.read_row(i, &mut buf).map(|(_, s)| s)
        };
        let n = Some(HudNames {
            p1: Name::new("synth-one"),
            p2: Name::new("synth-two"),
        });
        ui.watch_hud(true, facts(n, 3, false));
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), SHOW_HUD | SHOW_NAMES);
        assert_eq!(row_text(b, ROW_NAME_L), "1P synth-one");
        assert_eq!(row_text(b, ROW_NAME_R), "synth-two 2P");
        assert_eq!(row_text(b, ROW_HUD), "Watch delay 3");
        assert_eq!(row_style(b, ROW_HUD), Some(RowStyle::Small));
        assert_eq!(row_style(b, ROW_NAME_L), Some(RowStyle::Normal));
        ui.watch_hud(true, facts(n, 41, true));
        assert_eq!(row_text(b, ROW_HUD), "Watch delay 41 >>");
        ui.watch_hud(true, facts(n, 2, false));
        assert_eq!(row_text(b, ROW_HUD), "Watch delay 2", "印が残った");
        let with_stats = |ping, latency, rollback| WatchHudFacts {
            names: n,
            stats: Some(PeerStats::from_record(ping, latency, rollback)),
            delay_frames: 3,
            fast_forward: false,
        };
        ui.watch_hud(true, with_stats(14, 1, 2));
        assert_eq!(
            row_text(b, ROW_HUD),
            "ping 14ms Delay 1F Rollback 2 Watch delay 3"
        );
        ui.watch_hud(true, with_stats(15, 1, 0));
        assert_eq!(
            row_text(b, ROW_HUD),
            "ping 15ms Delay 1F Rollback 0 Watch delay 3"
        );
        ui.watch_hud(true, facts(None, 2, false));
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), SHOW_HUD);
        ui.watch_hud(true, facts(n, 2, false));
        ui.watch_hud(false, WatchHudFacts::default());
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), 0);
        ui.watch_hud(true, facts(n, 2, false));
        ui.after_watch_teardown();
        assert_eq!(b.show() & (SHOW_HUD | SHOW_NAMES), 0);
        ui.hud(true, hud_with(Some(names_of(Seat::P1))));
        assert!(row_text(b, ROW_HUD).starts_with("Delay"));
        assert_eq!(row_style(b, ROW_HUD), Some(RowStyle::Normal));
        ui.watch_hud(true, facts(n, 5, false));
        assert_eq!(row_text(b, ROW_HUD), "Watch delay 5");
        assert_eq!(row_style(b, ROW_HUD), Some(RowStyle::Small));
    }

    #[test]
    fn the_small_status_line_stays_left_of_the_fps_and_inside_its_band() {
        let s = spec();
        let (sw, sh) = (640u32, 480u32);
        // 小さい文字の枠（鍵の枠の左上 5 × 10）だけを不透明にした表: ふつうの文字の枠は透明のまま（取り違えれば墨が出ない）。
        let mut texels = vec![0u32; s.texel_count()];
        for c in higanr_core::overlay::ASCII
            .iter()
            .filter_map(|&c| higanr_core::overlay::small_key(c))
        {
            let (ox, oy) = s.cell_origin(s.slot_of(c).unwrap());
            for y in oy..oy + higanr_core::overlay::SMALL_CELL_H {
                for x in ox..ox + higanr_core::overlay::SMALL_CELL_W {
                    texels[(y * s.tex_w + x) as usize] = 0xFFFF_FFFF;
                }
            }
        }
        let longest = "ping 999ms Delay 99F Rollback 99 Watch delay 99 >>";
        let ink = |style: RowStyle, text: &str| {
            let b = board();
            b.set_row(ROW_HUD, text, style);
            b.set_show(SHOW_HUD);
            let mut canvas = vec![0u8; (sw * sh * 3) as usize];
            draw_board(b, &s, sw, sh, &mut |v| {
                blit_quads(v, &texels, s.tex_w, s.tex_h, &mut canvas, sw, sh, 0, 0);
            });
            let mut r: Option<(u32, u32, u32, u32)> = None;
            for y in 0..sh {
                for x in 0..sw {
                    let i = ((y * sw + x) * 3) as usize;
                    if canvas[i] == 255 {
                        r = Some(match r {
                            None => (x, y, x, y),
                            Some((a, b, c, d)) => (a.min(x), b.min(y), c.max(x), d.max(y)),
                        });
                    }
                }
            }
            r
        };
        let (x0, y0, x1, y1) = ink(RowStyle::Small, longest).expect("墨が出ない");
        let band_top = sh - HUD_BAND_H;
        assert!(
            (lay::HUD_EDGE_X..lay::HUD_EDGE_X + 5).contains(&x0),
            "左端 {x0}"
        );
        // 最後の文字（`>`）の右端がfpsの文字（x = 284）の手前（半角1文字ぶんの隙間を残す: `HUD_BOTTOM_RIGHT` = 277）に収まる。
        assert!(x1 < lay::HUD_BOTTOM_RIGHT, "右端 {x1} が fps に近すぎる");
        assert!(
            x1 + 1 >= lay::HUD_EDGE_X + 50 * 5,
            "最長の行が途中で切れた（右端 {x1}）"
        );
        assert!(
            y0 >= band_top && y1 < sh - 1,
            "墨の y {y0}..{y1}（帯 {band_top}..{sh}）"
        );
        assert!(
            y1 - y0 < higanr_core::overlay::SMALL_CELL_H,
            "墨が文字の高さを越える"
        );
        assert_eq!(ink(RowStyle::Normal, "ping 14ms"), None);
    }

    use higanr_core::end_outline::{RGB_1P, RGB_2P};

    fn rgb_of(c: u32) -> [u8; 3] {
        [(c >> 16) as u8, (c >> 8) as u8, c as u8]
    }

    #[test]
    fn the_name_bands_sit_at_both_top_corners_and_the_status_line_at_the_bottom_left() {
        let s = spec();
        let (sw, sh) = (640u32, 480u32);
        let texels = vec![0xFFFF_FFFFu32; s.texel_count()];
        let b = board();
        b.set_row(ROW_NAME_L, "1P ab", RowStyle::Normal);
        b.set_row(ROW_NAME_R, "cd 2P", RowStyle::Normal);
        b.set_row(ROW_HUD, "xy", RowStyle::Normal);
        b.set_show(SHOW_NAMES);
        let mut canvas = vec![0u8; (sw * sh * 3) as usize];
        let got = draw_board(b, &s, sw, sh, &mut |v| {
            blit_quads(v, &texels, s.tex_w, s.tex_h, &mut canvas, sw, sh, 0, 0);
        });
        assert_eq!(got, 2, "名前の旗だけなら名前の帯 2 つ");
        let px = |x: u32, y: u32| {
            let i = ((y * sw + x) * 3) as usize;
            [canvas[i], canvas[i + 1], canvas[i + 2]]
        };
        let white = [255, 255, 255];
        let (red, blue) = (rgb_of(RGB_1P), rgb_of(RGB_2P));
        let w_left = lay::text_width_plain(&s, "1P ab");
        let w_right = lay::text_width_plain(&s, "cd 2P");
        let w_label = lay::text_width_plain(&s, "1P");
        assert_eq!(px(lay::HUD_EDGE_X, HUD_Y), red, "1P の語は赤");
        assert_eq!(px(lay::HUD_EDGE_X + w_label - 1, HUD_Y), red);
        assert_eq!(
            px(lay::HUD_EDGE_X + w_label + 7, HUD_Y),
            white,
            "名前の文字は白のまま"
        );
        assert_eq!(
            px(lay::HUD_EDGE_X + w_left - 1, HUD_Y),
            white,
            "左の帯の最後の文字"
        );
        assert_eq!(
            px(lay::HUD_EDGE_X + w_left, HUD_Y),
            [0, 0, 0],
            "左の帯の文字が長い"
        );
        let right_end = sw - lay::HUD_EDGE_X;
        assert_eq!(
            px(right_end - 1, HUD_Y),
            blue,
            "右の帯の最後の文字が右端でない（2P の語は青）"
        );
        assert_eq!(px(right_end, HUD_Y), [0, 0, 0]);
        assert_eq!(
            px(right_end - lay::text_width_plain(&s, "2P"), HUD_Y),
            blue,
            "2P の語の最初の文字"
        );
        assert_eq!(
            px(right_end - lay::text_width_plain(&s, "2P") - 1, HUD_Y),
            [0, 0, 0],
            "名前と語の間の空白"
        );
        assert_eq!(px(right_end - w_right, HUD_Y), white, "右の帯の最初の文字");
        assert_eq!(px(right_end - w_right - 1, HUD_Y), [0, 0, 0]);
        assert_eq!(
            px(lay::HUD_EDGE_X, lay::hud_bottom_y(sh) + HUD_Y),
            [0, 0, 0]
        );
        b.set_show(SHOW_NAMES | SHOW_HUD);
        let mut n = 0;
        let mut canvas = vec![0u8; (sw * sh * 3) as usize];
        draw_board(b, &s, sw, sh, &mut |v| {
            n += 1;
            blit_quads(v, &texels, s.tex_w, s.tex_h, &mut canvas, sw, sh, 0, 0);
        });
        assert_eq!(n, 3);
        let i = ((((lay::hud_bottom_y(sh) + HUD_Y) * sw) + lay::HUD_EDGE_X) * 3) as usize;
        assert_eq!(&canvas[i..i + 3], &white);
    }

    /// 全面が白の文字の表で`b`を描き、白（255）の画素の外接の矩形`(x0, y0, x1, y1)`を返す（下の枠`y >= from_y`だけ）。
    fn white_ink(b: &Board, from_y: u32) -> Option<(u32, u32, u32, u32)> {
        let s = spec();
        let (sw, sh) = (640u32, 480u32);
        let texels = vec![0xFFFF_FFFFu32; s.texel_count()];
        let mut canvas = vec![0u8; (sw * sh * 3) as usize];
        draw_board(b, &s, sw, sh, &mut |v| {
            blit_quads(v, &texels, s.tex_w, s.tex_h, &mut canvas, sw, sh, 0, 0);
        });
        let mut r: Option<(u32, u32, u32, u32)> = None;
        for y in from_y..sh {
            for x in 0..sw {
                if canvas[((y * sw + x) * 3) as usize] == 255 {
                    r = Some(match r {
                        None => (x, y, x, y),
                        Some((a, b, c, d)) => (a.min(x), b.min(y), c.max(x), d.max(y)),
                    });
                }
            }
        }
        r
    }

    #[test]
    fn the_status_line_on_the_right_is_right_aligned_and_never_reaches_the_fps_text() {
        let s = spec();
        let top = lay::hud_bottom_y(480);
        let b = board();
        b.set_row(ROW_HUD, "xy", RowStyle::Normal);
        b.set_show(SHOW_HUD);
        assert_eq!(
            white_ink(b, top).map(|r| r.0),
            Some(lay::HUD_EDGE_X),
            "既定は左下"
        );
        b.set_hud_right(true);
        let w = lay::text_width_plain(&s, "xy");
        let (x0, y0, x1, y1) = white_ink(b, top).expect("右下に出ない");
        assert_eq!(
            (x0, x1),
            (640 - lay::HUD_EDGE_X - w, 640 - lay::HUD_EDGE_X - 1),
            "右端から HUD_EDGE_X あけて右寄せ"
        );
        assert_eq!((y0, y1), (top + HUD_Y, top + HUD_Y + 13));
        // 出しうる一番長い行（左下の上限いっぱい）でも、fpsの文字（x = 284..352）に重ならない。
        let longest = "Delay* 9 (9)  ping 4294967295ms  rollback 255  ".repeat(3);
        for right in [false, true] {
            b.set_row(ROW_HUD, &longest, RowStyle::Normal);
            b.set_hud_right(right);
            let (x0, _, x1, _) = white_ink(b, top).unwrap();
            let (lo, hi) = if right {
                (lay::HUD_BOTTOM_LEFT_MIN, 640 - lay::HUD_EDGE_X)
            } else {
                (lay::HUD_EDGE_X, lay::HUD_BOTTOM_RIGHT)
            };
            assert!(
                x0 >= lo && x1 < hi,
                "right={right}: 墨 {x0}..{x1} が {lo}..{hi} の外"
            );
            assert!(
                x1 < lay::FPS_LEFT || x0 > lay::FPS_RIGHT,
                "right={right}: 墨 {x0}..{x1} が fps（{}..{}）に重なる",
                lay::FPS_LEFT,
                lay::FPS_RIGHT
            );
            assert!(
                x1 - x0 + 1 >= 200,
                "right={right}: 長い行が途中で切れすぎ（{x0}..{x1}）"
            );
        }
    }

    #[test]
    fn a_banded_text_colors_only_the_accent_range_and_is_cut_by_the_total_width() {
        let s = spec();
        let (base, hot) = (0xFFFF_FFFFu32, SEAT_COLOR_1P);
        let mut out = [Vertex::default(); 120];
        let colors = |out: &[Vertex], n: usize| -> Vec<u32> {
            (0..n / 6).map(|q| out[q * 6].color).collect()
        };
        // 語の2文字が赤、空白は四角なし、名前の文字は白。
        let n = push_banded(
            &mut out,
            0,
            &s,
            (2, 1),
            "1P ab",
            (base, Some((0..2, hot))),
            1000,
        );
        assert_eq!(colors(&out, n), vec![hot, hot, base, base]);
        // 末尾の語（2Pの帯）。
        let n = push_banded(
            &mut out,
            0,
            &s,
            (2, 1),
            "ab 2P",
            (base, Some((3..5, hot))),
            1000,
        );
        assert_eq!(colors(&out, n), vec![base, base, hot, hot]);
        // 全体の幅で切る: 幅3文字ぶん（21）なら語の2文字と空白で終わり、名前は描かない。
        let n = push_banded(
            &mut out,
            0,
            &s,
            (2, 1),
            "1P ab",
            (base, Some((0..2, hot))),
            21,
        );
        assert_eq!(colors(&out, n), vec![hot, hot]);
        // 語の途中で切れたら後ろの部分を詰めて描かない。
        let n = push_banded(
            &mut out,
            0,
            &s,
            (2, 1),
            "ab 2P",
            (base, Some((3..5, hot))),
            20,
        );
        assert_eq!(colors(&out, n), vec![base, base]);
        // 範囲が文字の境目でない・逆なら色は付けない（全部ふつうの色）。
        let reversed = core::ops::Range { start: 2, end: 1 };
        for bad in [1..2, reversed, 0..9] {
            let n = push_banded(
                &mut out,
                0,
                &s,
                (2, 1),
                "あb",
                (base, Some((bad, hot))),
                1000,
            );
            assert_eq!(colors(&out, n), vec![base, base]);
        }
        let n = push_banded(&mut out, 0, &s, (2, 1), "ab", (base, None), 1000);
        assert_eq!(colors(&out, n), vec![base, base]);
    }

    #[test]
    fn the_status_side_setting_picks_the_side_from_the_own_seat_and_watchers_stay_left() {
        let hud = |seat| HudFacts {
            latency: Some(1),
            seat,
            ..HudFacts::default()
        };
        let mut checked = 0;
        for side in StatusSide::ALL {
            for seat in [Some(Seat::P1), Some(Seat::P2), None] {
                let (ui, _l, b) = new_ui();
                let mut ui = ui.with_status_side(side);
                assert!(!b.hud_right());
                ui.hud(true, hud(seat));
                assert_eq!(
                    b.hud_right(),
                    side.on_right(seat.unwrap_or(Seat::P1)),
                    "{side:?} {seat:?}"
                );
                checked += 1;
                // 観戦者の画面は設定によらず左下。
                ui.watch_hud(true, WatchHudFacts::default());
                assert!(!b.hud_right(), "{side:?}: 観戦者の行が右に出た");
                // 試合の外（消す）で置き場所を触らず、次の試合で設定に戻る。
                ui.hud(true, hud(seat));
                ui.hud(false, HudFacts::default());
                assert_eq!(b.hud_right(), side.on_right(seat.unwrap_or(Seat::P1)));
            }
        }
        assert_eq!(checked, 12);
    }

    // ---------------- 観戦の「ランダムマッチ」の一覧 ----------------

    #[test]
    fn the_watch_list_box_carries_a_whole_list_and_only_newer_ones() {
        use higanr_core::watch_list::{ListStatus, WatchSnapshot};
        let link = UiLink::new();
        let mut snap = Box::new(WatchSnapshot::EMPTY);
        let mut seen = link.watch_list_version();
        assert!(
            !link.read_watch_list(&mut snap, &mut seen),
            "何も置いていない"
        );
        let rows: Vec<_> = (0..100u8)
            .map(|i| watch_row(i + 1, &format!("P{i}"), "Bob", i % 11))
            .collect();
        link.publish_watch_list(ListStatus::Ready, &rows);
        assert!(link.read_watch_list(&mut snap, &mut seen));
        assert_eq!((snap.status, snap.len()), (ListStatus::Ready, 100));
        assert_eq!(snap.rows(), &rows[..]);
        assert!(
            !link.read_watch_list(&mut snap, &mut seen),
            "同じバージョンは読み直さない"
        );
        // 行が減る・状態だけ変わる・100件を越える分は捨てる。
        link.publish_watch_list(ListStatus::Failed, &[]);
        assert!(link.read_watch_list(&mut snap, &mut seen));
        assert_eq!((snap.status, snap.len()), (ListStatus::Failed, 0));
        let many: Vec<_> = (0..120u8).map(|i| watch_row(i + 1, "A", "B", 0)).collect();
        link.publish_watch_list(ListStatus::Ready, &many);
        assert!(link.read_watch_list(&mut snap, &mut seen));
        assert_eq!(snap.len(), 100);
        // 開いた時点のバージョンを覚えればそれ以前に設定された一覧は読まない。
        let mut seen = link.watch_list_version();
        assert!(!link.read_watch_list(&mut snap, &mut seen));
        link.publish_watch_list(ListStatus::Loading, &[]);
        assert!(link.read_watch_list(&mut snap, &mut seen));
        assert_eq!((snap.status, snap.len()), (ListStatus::Loading, 0));
    }

    #[test]
    fn a_listed_pick_reaches_the_opener_and_a_typed_name_clears_it() {
        let link = UiLink::new();
        assert_eq!(link.spectate_pick(), None);
        let row = watch_row(7, "Alice", "Bob", 2);
        link.send(Cmd::SpectateListed(row));
        assert_eq!(link.take_order(), Some(Order::Spectate));
        assert_eq!(link.spectate_pick(), Some(row));
        assert_eq!(
            link.spectate_name(),
            None,
            "一覧の選びがルーム名の入れ物に触った"
        );
        // 別の行を選び直せる。
        let other = watch_row(8, "Carol", "Dave", 0);
        link.send(Cmd::SpectateListed(other));
        assert_eq!(link.spectate_pick(), Some(other));
        // 観戦の「ルームマッチ」でルーム名を打ったら古い選びは残らない（その名で入る）。
        link.send(Cmd::Spectate(RoomBuf::from_name("typed-1").unwrap()));
        assert_eq!(link.spectate_pick(), None);
        assert_eq!(link.spectate_name(), RoomBuf::from_name("typed-1"));
        assert_eq!(link.take_order(), Some(Order::Spectate));
        assert_eq!(link.take_order(), Some(Order::Spectate));
        assert_eq!(link.take_order(), None);
    }

    static FEED_CALLS: AtomicU32 = AtomicU32::new(0);
    static FEED_ON: AtomicU32 = AtomicU32::new(0);

    fn feed_probe(on: bool) {
        FEED_CALLS.fetch_add(1, Ordering::SeqCst);
        FEED_ON.store(u32::from(on), Ordering::SeqCst);
    }

    #[test]
    fn the_watch_list_path_does_not_allocate_opens_the_feed_once_per_visit_and_picks_a_row() {
        use higanr_core::watch_list::ListStatus;
        FEED_CALLS.store(0, Ordering::SeqCst);
        FEED_ON.store(0, Ordering::SeqCst);
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b)
            .with_room(RoomItem::On(RoomBuf::from_name("init-name").unwrap()))
            .with_watch_feed(Some(feed_probe));
        let s = spec();
        let mut g = Quiet {
            screen: 6,
            cursor: 4,
            word: 0,
            im2: InputState::new(),
            fed: [0; 3],
            typed: [0; 8],
            typed_n: 0,
            gate: false,
        };
        let f = facts();
        let rows: Vec<_> = (0..30u8)
            .map(|i| {
                watch_row(
                    i + 1,
                    &format!("P{i}"),
                    "Bob",
                    if i == 1 { 10 } else { i % 5 },
                )
            })
            .collect();
        let tick = |ui: &mut NetUi, g: &mut Quiet, words: &[u16]| {
            for &w in words {
                g.word = w;
                let _ = ui.tick(g, &f);
            }
        };
        // 開く → 観戦の「ランダムマッチ」（上から3つ下）を決める。
        tick(&mut ui, &mut g, &[Z, 0, DOWN, 0, DOWN, 0, DOWN, 0, Z, 0]);
        assert!(ui.menu.wants_watch_list());
        assert_eq!(
            (
                FEED_CALLS.load(Ordering::SeqCst),
                FEED_ON.load(Ordering::SeqCst)
            ),
            (1, 1),
            "一覧の画面に入ったのに接続を開けない（1 回だけ）"
        );
        link.publish_watch_list(ListStatus::Ready, &rows);
        let mut draws = 0usize;
        let mut allocs = 0u64;
        for round in 0..2 {
            let before = crate::test_alloc::count();
            // 一覧が書き直されるたびに読む（接続のスレッドの代わりに書く。書くのは確保しない）→ 下へ何度も（満員の行は飛ぶ）→ 上へ → 描く。
            for w in std::iter::repeat_n([DOWN, 0], 40).flatten() {
                link.publish_watch_list(ListStatus::Ready, &rows);
                g.word = w;
                let _ = ui.tick(&mut g, &f);
            }
            for w in std::iter::repeat_n([UP, 0], 10).flatten() {
                g.word = w;
                let _ = ui.tick(&mut g, &f);
                draws += draw_board(b, &s, 640, 480, &mut |v| {
                    assert!(v.len() % 6 == 0);
                });
            }
            if round == 1 {
                allocs = crate::test_alloc::count() - before;
            }
        }
        assert_eq!(allocs, 0, "一覧の画面の経路で確保した");
        assert!(draws >= 20, "盤を描く経路を通っていない（{draws}）");
        assert_eq!(
            FEED_CALLS.load(Ordering::SeqCst),
            1,
            "画面の中で接続を開け直した"
        );
        // 行を決定するとその行が指示になって一覧の画面を出る（接続を閉じる）。満員の2行目は飛ばされる。
        let shown = menu_rows(b);
        let marked: Vec<&String> = shown.iter().filter(|l| l.starts_with("> ")).collect();
        assert_eq!(marked.len(), 1, "{shown:?}");
        let o = {
            g.word = Z;
            ui.tick(&mut g, &f)
        };
        assert!(
            o.spectate_open,
            "行を決めても観戦者のネットワークを開く頼みが出ない"
        );
        assert!(!o.open_net);
        assert_eq!(link.take_order(), Some(Order::Spectate));
        let pick = link.spectate_pick().expect("選んだ行");
        assert!(!pick.is_full(), "満員の行を選んだ");
        assert!(rows.contains(&pick));
        assert_eq!(
            (
                FEED_CALLS.load(Ordering::SeqCst),
                FEED_ON.load(Ordering::SeqCst)
            ),
            (2, 0),
            "行を選んだ後に接続を閉じない"
        );
        assert!(!ui.menu.wants_watch_list());
    }

    #[test]
    fn leaving_the_watch_list_by_cancel_closes_the_feed_and_forgets_the_rows() {
        use higanr_core::watch_list::ListStatus;
        FEED_CALLS.store(0, Ordering::SeqCst);
        let link = Arc::new(UiLink::new());
        let b = board();
        let mut ui = NetUi::new(ini(), Arc::clone(&link), b)
            .with_room(RoomItem::On(RoomBuf::new()))
            .with_watch_feed(Some(feed_probe));
        let mut g = Fake::on_match_screen(4);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        for _ in 0..3 {
            g.step(&mut ui, &facts(), DOWN);
            g.step(&mut ui, &facts(), 0);
        }
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        assert!(ui.menu.wants_watch_list());
        // 取得中の間は「取得しています」だけ。サーバーから一覧が届くと行が出る。
        let t = menu_rows(b);
        assert!(
            t.iter().any(|l| l.contains("一覧を取得しています。")),
            "{t:?}"
        );
        link.publish_watch_list(ListStatus::Ready, &[watch_row(1, "Alice", "Bob", 3)]);
        g.step(&mut ui, &facts(), 0);
        assert!(
            menu_rows(b)
                .iter()
                .any(|l| l.contains("Alice") && l.contains("3/10"))
        );
        // キャンセル: 接続を閉じる。次に開いたときは前の行を見せない（取得中から）。
        g.step(&mut ui, &facts(), X);
        g.step(&mut ui, &facts(), 0);
        assert!(!ui.menu.wants_watch_list() && ui.menu_active());
        assert_eq!(FEED_CALLS.load(Ordering::SeqCst), 2);
        g.step(&mut ui, &facts(), Z);
        g.step(&mut ui, &facts(), 0);
        assert!(ui.menu.wants_watch_list());
        assert_eq!(FEED_CALLS.load(Ordering::SeqCst), 3);
        let t = menu_rows(b);
        assert!(
            !t.iter().any(|l| l.contains("Alice")),
            "前の行が残った: {t:?}"
        );
        assert!(
            t.iter().any(|l| l.contains("一覧を取得しています。")),
            "{t:?}"
        );
        // 前に書いた一覧がバッファに残っていても開き直した後に設定されたものだけを読む。
        g.step(&mut ui, &facts(), 0);
        assert!(!menu_rows(b).iter().any(|l| l.contains("Alice")));
    }
}
