//! 観戦のホストの側: 観戦用の接続コードを作り、受信スレッドの中でリングバッファ（`spectate_ring.rs`）から記録を取り出して `SpectateHost` に
//! 積み、観戦者へ送る。
//!
//! ```text
//! ゲームのスレッド → SpectateRing::push ──(原子変数のリングバッファ)──→ 受信スレッド: SpectateSender::tick
//!                                                              │ SpectateHost::push → poll（STREAM の送り・送り直し・外す）
//! 観戦者の JOIN / ACK / BYE ──→ 受信スレッド: SpectateSender::on_datagram → SpectateHost::on_datagram
//! ```
//!
//! - 送りは全部**受信スレッド**（`net_udp::recv_loop`。PONG を返す `answer_ping` と同じ場所）。ゲームのスレッドはリングバッファへ書くだけで、
//!   観戦者が遅れても黙っても消えても待たない（対戦者への遅延の増分 0）。
//! - 観戦のデータグラムは、ヘッダの session_id が観戦用のコードのもの（対戦とは**別に**引く。`code::Side::Spectate`）だけを
//!   観戦の側へ回す（`net_udp`）。対戦のセッション（`link`）には渡さない。
//! - 観戦用のコードは `spectate_code.txt`（接続コードの置き場と同じ）へ書く。種別が 3 なので、対戦のコードとして貼っても
//!   `WrongSide` で断られる。
//!
//! 観戦者への遅れ: リングバッファを空にして送る周回は、対戦の相手のパケットと観戦者の ACK が届くたびと、受け待ち（`net_udp::RECV_POLL` の
//! 50 ms）が切れたときに回る。ゲームのスレッドが 60 Hz で 300 記録を書き、対戦の相手のパケットが届く間は p50 8.4 ms・p95 15.8 ms・
//! 最大 19.4 ms、届かないと p50 30.1 ms・p95 58.2 ms・最大 62.0 ms（実測。n = 300 ずつ。loopback）。対戦者には影響しない。
//! もっと細かくするなら `net_win::Waiter`（高分解能の待機タイマー）で待つ別のスレッドが要る。
//!
//! 穴あけ: 配る側のスレッドが観戦者の返しを確かめて `Shared::request_punch` で頼むと、受信スレッドが次の周回（`tick`）で世代の
//! 原子変数 1 つを見て `PunchPlan` を持ち替え、観戦者の候補へ 4 B の印を送る。観戦者の JOIN（タグが合う）が来た送り元への穴あけは
//! 止める。送信ソケットのガード（`SockOut`）はネットワークの範囲の外へ送らない。ゲームのスレッドは関わらない。
//!
//! 未対応（ini の `[spectate]` の経路）: コードの期限が切れた後の書き直し・STUN で見えたアドレスを候補に載せること（LAN / loopback
//! だけ）・ローダーのウィンドウへの受け渡し。ゲーム内のメニューの経路（`code_side = game`）は `spectate_offer::arm` が STUN のアドレスも載せ、
//! 入るたびに新しい期限のコードをルームサーバーへ預ける。
#![forbid(unsafe_code)]

use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use higanr_core::auth::Secret;
use higanr_core::candidate::Candidates;
use higanr_core::code::{self, Code};
use higanr_core::guard::{self, Scope};
use higanr_core::handshake::Name;
use higanr_core::link::Outlet;
use higanr_core::spectate::{HostConfig, HostCounts, HostEvent, PunchPlan, SpectateHost};
use higanr_core::wire::{ByeReason, Record};

use crate::net_config::NetSettings;
use crate::net_udp::RecvThread;
use crate::netplay::NetPlay;
use crate::spectate_offer::{OfferCounts, OfferShared};
use crate::spectate_ring::{RingCounts, SpectateRing};

/// 観戦用のコードのファイル名（置き場は接続コードと同じ）。
pub const SPECTATE_CODE_FILE: &str = "spectate_code.txt";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SendCounts {
    pub sent: u32,
    pub refused: u32,
    pub errors: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PunchCounts {
    /// 配る側の頼み（`Shared::request_punch`）を受けて、穴あけの予定を作った回数。
    pub started: u32,
    /// 実際に送った穴あけのパケットの数（ネットワークの範囲の外でガードが止めた分は `SendCounts::refused`）。
    pub sent: u32,
    /// 観戦者の JOIN が来て、そのアドレスへの穴あけを止めた回数。
    pub stopped: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub host: HostCounts,
    pub spectators: u32,
    pub ring: RingCounts,
    pub send: SendCounts,
    pub punch: PunchCounts,
    /// 配る側（ルームの中継。`spectate_offer.rs`）の数。配る側の用意が無い実行（`armed` が false）では行に出さない。
    pub offer: OfferCounts,
    /// 観戦のホストの中でパニックして、受信スレッドが観戦のホストを捨てた（`net_udp::run_spectate`。数えは最後の値のまま）。
    pub stopped: bool,
}

impl Snapshot {
    pub fn text(&self) -> String {
        let h = &self.host;
        let r = &self.ring;
        let s = &self.send;
        let line = format!(
            "観戦: 観戦者 {} 人（入った {} / 上限で断った {} / 自分で抜けた {} / 黙って外した {} / リングバッファの溢れで全員外した {}） / 積んだ記録 {}（ゲームのリングバッファで捨てた {}・欠け {} 回） / 送った記録 {}（{} 通・送り直し {} 通・送り直した記録 {}・生きている合図 {}・リングバッファから落ちて飛ばした {}） / 受けた ACK {}（積んだより先の嘘 {}・受け取れた範囲の嘘 {}） / 捨てた受信（HGRT でない {}・別の session {}・タグ {}・読めない {}・バージョン違い {}・受けない type {}・知らない観戦者 {}） / 送り口 {} 通（ガードで止めた {}・誤り {}）",
            self.spectators,
            h.joined,
            h.refused_full,
            h.left,
            h.silenced,
            h.overflows,
            h.pushed,
            r.dropped_full + r.dropped_gap,
            r.gaps,
            h.records_sent,
            h.packets_sent,
            h.resends,
            h.records_resent,
            h.heartbeats,
            h.jumps,
            h.acks,
            h.acks_clamped,
            h.sacks_clamped,
            h.not_hgrt,
            h.wrong_session,
            h.bad_tag,
            h.undecodable,
            h.other_version,
            h.unexpected,
            h.unknown_spectator,
            s.sent,
            s.refused,
            s.errors,
        );
        // 配る側の用意が無い実行（ini の `[spectate]` の LAN / loopback の経路）は、今までと同じ行のまま。
        let offer = if self.offer.armed {
            let p = &self.punch;
            format!(
                "{} / 穴あけ（予定を作った {}・送った {} 通・観戦者の JOIN で止めた {}）",
                self.offer.text(),
                p.started,
                p.sent,
                p.stopped
            )
        } else {
            String::new()
        };
        let stopped = if self.stopped {
            " / パニックで観戦を止めた（対戦は続く）"
        } else {
            ""
        };
        format!("{line}{offer}{stopped}")
    }
}

/// `Snapshot` の置き場（受信スレッドが書き、読み取りのスレッドが読む。錠は数百バイトのコピーの間だけ）と、配る側のスレッドから
/// 受信スレッドへの穴あけの頼み。
#[derive(Debug, Default)]
pub struct Shared {
    snap: Mutex<Snapshot>,
    /// 穴あけの頼み（配る側のスレッドが置き、受信スレッドが取る。最後の 1 件だけ持つ）。錠は置く・取る間だけ。
    punch: Mutex<Option<Candidates>>,
    /// 頼みの世代。配る側が頼みを置いた後に進め、受信スレッドは周回ごとにこの原子変数 1 つだけを見る（錠は世代が変わったときだけ）。
    punch_gen: AtomicU32,
    /// 配る側のスレッドの共有（数・止める印・持ち手）。
    pub(crate) offer: OfferShared,
    /// 検査だけ: 受信スレッドの 1 周で観戦のホストに使った時間（負荷の測り）。
    #[cfg(test)]
    pub(crate) busy: BusyLog,
}

/// 受信スレッドが 1 周ごとに観戦のホストへ使った時間（ns）。`dgram` は観戦者の便を処理した分（ある周だけ）、`round` はその周の
/// 観戦の仕事の合計（便の処理 ＋ `tick`）。この間に着いた対戦の相手の便は、OS のバッファで待つ。
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct BusyLog {
    pub dgram: Mutex<Vec<u32>>,
    pub round: Mutex<Vec<u32>>,
}

impl Shared {
    /// 集計。配る側の数は原子変数から読み直す（配る側のスレッドが書き、受信スレッドのコピーには入っていない）。
    pub fn get(&self) -> Snapshot {
        let mut s = *self.snap.lock().unwrap_or_else(|e| e.into_inner());
        s.offer = self.offer.counts();
        s
    }

    fn set(&self, s: Snapshot) {
        *self.snap.lock().unwrap_or_else(|e| e.into_inner()) = s;
    }

    /// 配る側のスレッド: 観戦者の候補への穴あけを頼む（`candidates` はネットワークの範囲で絞ってあること）。受信スレッドが次の周回で
    /// 見て、今の予定を持ち替える。
    pub fn request_punch(&self, candidates: Candidates) {
        *self.punch.lock().unwrap_or_else(|e| e.into_inner()) = Some(candidates);
        // 値を置いた後に進める（読み手は世代が変わったのを見てから錠を取る）。
        self.punch_gen.fetch_add(1, Ordering::Release);
    }

    /// 受信スレッド: `seen` より新しい頼みがあれば取る（錠が取れなければ次の周回。待たない）。
    fn take_punch(&self, seen: &mut u32) -> Option<Candidates> {
        let generation = self.punch_gen.load(Ordering::Acquire);
        if generation == *seen {
            return None;
        }
        let mut g = match self.punch.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        *seen = generation;
        g.take()
    }

    /// 検査だけ: 観戦者の数を置く（観戦のホストの代わり）。
    #[cfg(test)]
    pub(crate) fn set_spectators(&self, n: u32) {
        self.snap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spectators = n;
    }

    pub fn stop_offer(&self) {
        self.offer.stop_and_join();
    }

    #[cfg(test)]
    pub fn offer_stopped(&self) -> bool {
        self.offer.stopped()
    }

    #[cfg(test)]
    pub fn punch_generation(&self) -> u32 {
        self.punch_gen.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub fn peek_punch(&self) -> Option<Candidates> {
        *self.punch.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 受信スレッドの送信ソケット。宛先がネットワークの範囲の中かを送る直前に見る（`GameNet::may_send` と同じ関数・同じ範囲）。
struct SockOut<'a> {
    sock: &'a UdpSocket,
    scope: Scope,
    counts: &'a mut SendCounts,
}

impl Outlet for SockOut<'_> {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        if !guard::allows(self.scope, to) {
            self.counts.refused = self.counts.refused.saturating_add(1);
            return;
        }
        match self.sock.send_to(bytes, to) {
            Ok(_) => self.counts.sent = self.counts.sent.saturating_add(1),
            Err(_) => self.counts.errors = self.counts.errors.saturating_add(1),
        }
    }
}

pub struct SpectateSender {
    host: SpectateHost,
    ring: Arc<SpectateRing>,
    scope: Scope,
    session_id: u32,
    send: SendCounts,
    /// 観戦者の NAT へ穴をあける予定（配る側の頼みで持ち替える。頼みが無い間は何も送らない）。
    punch: PunchPlan,
    punch_seen: u32,
    punch_counts: PunchCounts,
    published: Option<Snapshot>,
    shared: Arc<Shared>,
    /// 検査だけ: この周の `on_datagram` に使った時間（ns。次の `tick` が `BusyLog::round` に足して戻す）。
    #[cfg(test)]
    carry_ns: u32,
    /// 検査だけ: 次の `tick` でパニックする（受信スレッドが観戦だけを止めて対戦の受信を続けるかを見る）。
    #[cfg(test)]
    pub(crate) panic_on_tick: bool,
}

impl SpectateSender {
    pub fn new(
        cfg: HostConfig,
        ring: Arc<SpectateRing>,
        scope: Scope,
        shared: Arc<Shared>,
    ) -> Self {
        Self::with_host(SpectateHost::new(cfg), cfg.session_id, ring, scope, shared)
    }

    pub fn with_host(
        host: SpectateHost,
        session_id: u32,
        ring: Arc<SpectateRing>,
        scope: Scope,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            host,
            ring,
            scope,
            session_id,
            send: SendCounts::default(),
            punch: PunchPlan::none(),
            punch_seen: 0,
            punch_counts: PunchCounts::default(),
            published: None,
            shared,
            #[cfg(test)]
            carry_ns: 0,
            #[cfg(test)]
            panic_on_tick: false,
        }
    }

    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    pub fn on_datagram(&mut self, sock: &UdpSocket, from: SocketAddr, bytes: &[u8], now: u32) {
        #[cfg(test)]
        let began = std::time::Instant::now();
        let mut out = SockOut {
            sock,
            scope: self.scope,
            counts: &mut self.send,
        };
        let ev = self.host.on_datagram(from, bytes, now, &mut out);
        // タグの合う JOIN が来た = その観戦者までの穴が開いた。そのアドレスへの穴あけは止める（残りの候補へは続ける）。
        if matches!(ev, HostEvent::Joined | HostEvent::Refreshed) && self.punch.stop_for(from) {
            self.punch_counts.stopped = self.punch_counts.stopped.saturating_add(1);
        }
        self.publish();
        #[cfg(test)]
        {
            self.carry_ns = u32::try_from(began.elapsed().as_nanos()).unwrap_or(u32::MAX);
            self.shared
                .busy
                .dgram
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(self.carry_ns);
        }
    }

    /// リングバッファを空にして `SpectateHost` に積み、送るものを送る。受信スレッドの周回ごと（受けた 1 通の後と、待ちの上限の後）。
    pub fn tick(&mut self, sock: &UdpSocket, now: u32) {
        #[cfg(test)]
        if self.panic_on_tick {
            panic!("観戦のホストの検査用のパニック");
        }
        #[cfg(test)]
        let began = std::time::Instant::now();
        while let Some(p) = self.ring.pop() {
            if p.after_gap {
                // リングバッファが溢れて欠けた後の最初の `Seg`。観戦者に「ここで切れた」を知らせてから新しい seg を積む。
                let _ = self.host.push(Record::End(ByeReason::Cancelled));
            }
            // 積めなかった（ホストのリングバッファが 1 seg で埋まった）分は `HostCounts` が数える。対戦は止めない。
            let _ = self.host.push(p.rec);
        }
        // 配る側の頼み（周回ごとに原子変数 1 つを見るだけ。頼みが立った周回だけ錠を取る）。
        if let Some(c) = self.shared.take_punch(&mut self.punch_seen) {
            self.punch = PunchPlan::new(&c, now);
            self.punch_counts.started = self.punch_counts.started.saturating_add(1);
        }
        let mut out = SockOut {
            sock,
            scope: self.scope,
            counts: &mut self.send,
        };
        self.host.poll(now, &mut out);
        let n = self.punch.poll(now, &mut out);
        self.punch_counts.sent = self
            .punch_counts
            .sent
            .saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
        self.publish();
        #[cfg(test)]
        {
            let tick = u32::try_from(began.elapsed().as_nanos()).unwrap_or(u32::MAX);
            let round = tick.saturating_add(std::mem::take(&mut self.carry_ns));
            self.shared
                .busy
                .round
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(round);
        }
    }

    fn publish(&mut self) {
        let snap = Snapshot {
            host: self.host.counts(),
            spectators: self.host.spectator_count() as u32,
            ring: self.ring.counts(),
            send: self.send,
            punch: self.punch_counts,
            // 配る側の数は `Shared::get` が原子変数から足す（配る側のスレッドが書くので、ここでコピーしない）。
            offer: OfferCounts::default(),
            stopped: false,
        };
        if self.published != Some(snap) {
            self.shared.set(snap);
            self.published = Some(snap);
        }
    }
}

pub struct Prepared {
    pub ring: Arc<SpectateRing>,
    pub sender: SpectateSender,
    /// 観戦用の接続コード（`spectate_code.txt` の中身。末尾の改行は書く側が足す）。
    pub code_text: String,
    pub note: String,
}

/// 観戦用のコードの session を引く。対戦の session（`peer` の形なら ini の値）と同じ値は避ける（同じだと、対戦の相手の
/// パケットが観戦のホストへ回る —— `net_udp` は session で分ける）。
pub(crate) fn pick_session(
    draw: &mut impl FnMut() -> Result<u32, String>,
    match_session: Option<u32>,
) -> Result<u32, String> {
    for _ in 0..8 {
        let s = draw()?;
        if Some(s) != match_session {
            return Ok(s);
        }
    }
    Err("観戦用の session を対戦の session と別に引けない".to_string())
}

/// 乱数を注入して用意する（検査）。`local` は自分の受信ソケット（観戦用のコードの候補に載せる）、`now_unix` はコードの期限の起点。
pub fn prepare_with(
    s: &NetSettings,
    local: SocketAddr,
    now_unix: u64,
    mut draw_session: impl FnMut() -> Result<u32, String>,
    draw_secret: impl FnOnce() -> Result<Secret, String>,
) -> Result<Prepared, String> {
    let session_id = pick_session(&mut draw_session, s.session_id)?;
    let secret = draw_secret()?;
    // 観戦用の使い捨ての鍵: 観戦を開くたびに OS の乱数で引き、公開鍵を観戦用のコードに載せる。コードを書く前に引くので、
    // 配ったコードの公開鍵は必ず配る側のセッションの秘密鍵と対になる。
    let host = HostConfig {
        session_id,
        secret,
        key_seed: crate::net_udp::draw_key_seed()?,
    };
    let candidates = NetSettings::own_candidates(local, None, None)?;
    let code_text = code::write_spectate(
        &Code {
            side: code::Side::Spectate,
            session_id,
            secret: Some(secret),
            name: Name::new(&s.name),
            nat: higanr_core::nat::NatMark::UNKNOWN,
            candidates,
            expires: now_unix.saturating_add(code::DEFAULT_TTL_SECS),
        },
        &host.public(),
    )
    .map_err(|e| format!("観戦用のコードを書けない: {e:?}"))?;
    let ring = Arc::new(SpectateRing::new());
    let sender = SpectateSender::new(
        host,
        Arc::clone(&ring),
        s.scope,
        Arc::new(Shared::default()),
    );
    Ok(Prepared {
        ring,
        sender,
        code_text,
        note: format!(
            "観戦のホストを用意した（観戦用の session 0x{session_id:08X}・候補 {local}・{SPECTATE_CODE_FILE} の期限 {}）",
            code::span_text(code::DEFAULT_TTL_SECS)
        ),
    })
}

pub fn prepare(s: &NetSettings, local: SocketAddr) -> Result<Prepared, String> {
    prepare_with(
        s,
        local,
        crate::net_udp::now_unix(),
        || {
            let mut b = [0u8; 4];
            if !crate::net_win::os_random(&mut b) {
                return Err("観戦用の session を OS の乱数で引けない".to_string());
            }
            Ok(u32::from_le_bytes(b))
        },
        crate::net_udp::draw_secret,
    )
}

pub fn write_code_file(dir: &Path, code_text: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{} を作れない: {e}", dir.display()))?;
    crate::net_code::replace_file(dir, SPECTATE_CODE_FILE, &format!("{code_text}\r\n"))
}

pub struct Pending {
    sender: SpectateSender,
    code_text: String,
    dir: PathBuf,
    note: String,
}

/// 観戦を使う設定なら（`[spectate] enable = 1`）、リングバッファを `np` に置き、置き場 `dir` と自分の受信ソケット `local` から観戦用のコードを用意する
/// （読み取りのスレッド。ネットワークのセッションをゲームへ渡す前）。戻りの文は本ログの 1 行に足す（観戦を使わない設定なら空）。用意できなくても
/// 対戦は開く（観戦だけを使わない。理由は文に出す）。
pub fn arm(
    s: &NetSettings,
    dir: Option<PathBuf>,
    local: Option<SocketAddr>,
    np: &mut NetPlay,
) -> (Option<Pending>, String) {
    if s.spectate.is_none() {
        return (None, String::new());
    }
    let (Some(dir), Some(local)) = (dir, local) else {
        return (
            None,
            " / 観戦は使わない（置き場か受信ソケットのアドレスが分からない）".to_string(),
        );
    };
    match prepare(s, local) {
        Ok(p) => {
            np.set_spectate(Arc::clone(&p.ring));
            (
                Some(Pending {
                    sender: p.sender,
                    code_text: p.code_text,
                    dir,
                    note: p.note,
                }),
                String::new(),
            )
        }
        Err(e) => (None, format!("{NOT_USED}{e}")),
    }
}

/// ネットワークを開いた文（`arm_net` の戻り）に足す、観戦の準備の失敗の句。`note_has_problem` が見分ける。
const NOT_USED: &str = " / 観戦は使わない: ";
const NOT_INSTALLED: &str = " / 観戦: 受信スレッドに置けなかった（もう置いてある）";
const CODE_NOT_WRITTEN: &str = "。ただしコードを書けない: ";

/// ネットワークを開いた文に、観戦の準備の失敗の句が混ざっているか（成功の文は詳細だけ、失敗は簡潔にも書くため）。
pub fn note_has_problem(desc: &str) -> bool {
    [NOT_USED, NOT_INSTALLED, CODE_NOT_WRITTEN]
        .iter()
        .any(|m| desc.contains(m))
}

impl Pending {
    /// ネットワークのセッションをゲームへ渡せた後に、送り手を受信スレッドへ置き、観戦用のコードを書く。戻りの文は本ログの 1 行に足す。
    /// コードを書けなくても送り手は置く（対戦は止めない。観戦者が繋げないだけ）。
    pub fn commit(self, th: &RecvThread) -> String {
        if !th.install_spectate(self.sender) {
            return NOT_INSTALLED.to_string();
        }
        match write_code_file(&self.dir, &self.code_text) {
            Ok(()) => format!(
                " / {}。{SPECTATE_CODE_FILE} を {} へ書いた",
                self.note,
                self.dir.display()
            ),
            Err(e) => format!(" / {}{CODE_NOT_WRITTEN}{e}", self.note),
        }
    }
}

#[cfg(test)]
mod load;
#[cfg(test)]
mod tests;
