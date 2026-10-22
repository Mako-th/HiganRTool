//! 観戦のホストの側: 観戦用の接続コードを作り、受信スレッドの中でリングバッファ（`spectate_ring.rs`）から記録を取り出して `SpectateHost` に積み、観戦者へ送る。
//!
//! - 送りは全部**受信スレッド**（`net_udp::recv_loop`。PONG を返す `answer_ping` と同じ場所）。ゲームのスレッドはリングバッファへ書くだけで、観戦者が遅れても黙っても消えても待たない（相手への遅延の増分 0）。
//! - ヘッダの session_id が観戦用のコードのもの（対戦とは**別に**引く。`code::Side::Spectate`）だけを観戦の側へ回す（`net_udp`）。観戦用のコードは `spectate_code.txt`（接続コードと同じフォルダ）へ書く。種別が 3 なので、対戦のコードとして貼っても `WrongSide` で断られる。
//! - IPv6: 観戦者の便は IPv4・IPv6 のどちらの受信スレッドにも届く。送り手は `net_udp::SpectateSlot` の錠の中に 1 つで、両方が使う（どちらも錠を待たない）。返事と穴あけは宛先の族のソケットから出す（`Socks`）。
//! - 観戦者への遅れ: リングバッファを空にして送る周回は、相手のパケットと観戦者の ACK が届くたびと、受け待ち（`net_udp::RECV_POLL` の 50 ms）が切れたときに回る。相手のパケットが届かないと遅れは最大 62.0 ms（実測。n = 300、loopback。届く間は最大 19.4 ms）。相手には影響しない。
//! - 穴あけ: 配る側のスレッドが観戦者の返しを検証して `Shared::request_punch` で頼むと、受信スレッドが次の周回（`tick`）で世代の原子変数 1 つを見て、観戦者ごとの `PunchPlan`（`PUNCH_PLANS` 人分）を作り、その人の候補へ 4 B の印を送る。観戦者の JOIN（タグが合い、入れた・紹介した）が来た送り元への穴あけは止める。送信ソケットのガード（`SockOut`）はネットワークの範囲の外へ送らない。
//! - 上りの測り: 観戦者の PAIR（1,200 B の組）は、受信スレッドが読んだ時刻（µs）のまま `SpectateSender::on_recv` が `SpectateHost::on_pair` へ渡す。
//!
//! 未対応（ini の `[spectate]` の経路）: コードの期限が切れた後の書き直し・STUN で見えたアドレスを候補に載せること（LAN / loopback だけ）・ローダーのウィンドウへの受け渡し。ゲーム内のメニューの経路（`code_side = game`）は `spectate_offer::arm` が STUN のアドレスも載せ、マッチのホストが観戦用のルームに入るたびに新しい期限のコードをルームサーバーへ預ける。
#![forbid(unsafe_code)]

use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use higanr_core::auth::Secret;
use higanr_core::candidate::{Candidate, Candidates, same_endpoint};
use higanr_core::code::{self, Code};
use higanr_core::guard::{self, Scope};
use higanr_core::handshake::Name;
use higanr_core::link::Outlet;
use higanr_core::spectate::{
    HostConfig, HostCounts, HostEvent, MAX_SPECTATORS, PunchPlan, SpectateHost,
};
use higanr_core::spectate_rate::ThrottleCounts;
use higanr_core::wire::{self, ByeReason, PeekedType, Record};

use crate::net_config::NetSettings;
use crate::net_udp::RecvThread;
use crate::netplay::NetPlay;
use crate::spectate_offer::{OfferCounts, OfferShared};
use crate::spectate_ring::{RingCounts, SpectateRing};

/// 本ログの木・上りの測りの行の先頭（`Snapshot::tree_text` / `measure_text`。接続の終わりの節がこの先頭で行を選ぶ）。
pub const TREE_ROW_HEAD: &str = "観戦者のツリー:";
pub const MEASURE_ROW_HEAD: &str = "上り回線速度の測定:";

/// 観戦用のコードのファイル名（保存先は接続コードと同じ）。
pub const SPECTATE_CODE_FILE: &str = "spectate_code.txt";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SendCounts {
    pub sent: u32,
    /// `sent` のうち IPv6 の宛先へ送った分。
    pub sent_v6: u32,
    pub refused: u32,
    pub errors: u32,
}

/// 観戦のデータグラムを受けた数（送り元の族ごと）。観戦者の JOIN・ACK がどちらの経路で届いているかを見る。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViaCounts {
    pub recv_v4: u32,
    pub recv_v6: u32,
    /// 錠待ちの列(`net_udp::SpectateSlot`)が溢れて捨てた便の数。受信スレッドで数えるので `SpectateSender` は書かない。
    pub queue_dropped: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PunchCounts {
    /// 配る側の頼み（`Shared::request_punch`）を受けて、穴あけの予定を作った回数。
    pub started: u32,
    /// 実際に送った穴あけのパケットの数（ネットワークの範囲の外でガードが止めた分は `SendCounts::refused`）。
    pub sent: u32,
    /// 観戦者の JOIN が来て、そのアドレスへの穴あけを止めた回数。
    pub stopped: u32,
    /// 予定の枠が全部まだ送っている途中で、いちばん先まで送った予定を押し出した回数。
    pub evicted: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub host: HostCounts,
    pub spectators: u32,
    /// `spectators` のうち IPv6 の送り元で入っている人。
    pub spectators_v6: u32,
    /// `spectators` のうち中継役に選んでいる人。
    pub relays: u32,
    pub ring: RingCounts,
    pub send: SendCounts,
    pub via: ViaCounts,
    pub punch: PunchCounts,
    /// 配る側（ルームの中継。`spectate_offer.rs`）の数。配る側の用意が無い実行（`armed` が false）では行に出さない。
    pub offer: OfferCounts,
    /// 観戦のホストの中でパニックして、受信スレッドが観戦のホストを捨てた（`net_udp::run_spectate`。数えは最後の値のまま）。
    pub stopped: bool,
    /// 対戦の往復の延びによる観戦者へのパケット送信の調整。
    pub rate: RateSnap,
}

/// 観戦者へのパケット送信の調整の集計（`spectate_rate::Throttle`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RateSnap {
    pub throttle: ThrottleCounts,
    /// 全体の上限に今掛けている割合（ppm）と、追いついている観戦者への送りも止めているか。
    pub level_ppm: u32,
    pub paused: bool,
    /// 受信スレッドに届いた、時刻の違う往復の要約の数。
    pub delays: u64,
}

impl Snapshot {
    /// 本ログの観戦の集計の行。木と測りの行は数が 1 つでもあるときだけ続ける。`detail` なら、観戦者へのパケット送信の調整の行も続ける（詳細のログだけの行）。
    pub fn lines(&self, detail: bool) -> String {
        let mut out = self.text();
        for extra in [self.tree_text(), self.measure_text()]
            .into_iter()
            .flatten()
        {
            out.push('\n');
            out.push_str(&extra);
        }
        if detail {
            out.push('\n');
            out.push_str(&self.rate_text());
        }
        out
    }

    /// 中継の木の行。紹介・依頼・取り置きなどの数が全部 0 で中継役もいなければ None。
    pub fn tree_text(&self) -> Option<String> {
        let h = &self.host;
        let counts = [
            h.intros,
            h.intro_resends,
            h.admit_resends,
            h.intros_expired,
            h.not_admitted,
            h.rehomes_sent,
            h.reserved_used,
            h.descendants_clamped,
            h.relay_jumps,
            h.punches_sent,
            h.swaps,
            h.swap_freed,
            u64::from(self.relays),
        ];
        if counts.iter().all(|&n| n == 0) {
            return None;
        }
        let swaps = if h.swaps + h.swap_freed > 0 {
            format!(
                " / マッチの切れ目の入れ替え {}（空いた直接接続の枠 {}）",
                h.swaps, h.swap_freed
            )
        } else {
            String::new()
        };
        Some(format!(
            "{TREE_ROW_HEAD} 中継役 {} 人 / 中継役への振り分け {}（再送 {}・期限切れ {}） / 受け入れ依頼の再送 {} / 確保済みの枠での受け入れ {} / 中継役が子へ送った付け替え指示 {} / 中継役の記録の積み直し {} / 子の数の申告の上限超え {} / 依頼のない JOIN {} / 外した中継役の子へ送った穴あけパケット {}{swaps}",
            self.relays,
            h.intros,
            h.intro_resends,
            h.intros_expired,
            h.admit_resends,
            h.reserved_used,
            h.rehomes_sent,
            h.relay_jumps,
            h.descendants_clamped,
            h.not_admitted,
            h.punches_sent,
        ))
    }

    /// 上りの測りの行。番を渡した数・受けた PAIR・見積もりが全部 0 なら None。
    pub fn measure_text(&self) -> Option<String> {
        let h = &self.host;
        if [
            h.measures,
            h.measure_resends,
            h.uplinks_measured,
            h.uplinks_unmeasured,
            h.pairs,
            h.pairs_dropped,
        ]
        .iter()
        .all(|&n| n == 0)
        {
            return None;
        }
        Some(format!(
            "{MEASURE_ROW_HEAD} 観戦者へ測定依頼を送った {}（測定依頼の再送 {}） / 速度の確定 {}・測定不能 {} / 観戦者から受けた測定パケット {}・捨てた測定パケット {}",
            h.measures,
            h.measure_resends,
            h.uplinks_measured,
            h.uplinks_unmeasured,
            h.pairs,
            h.pairs_dropped,
        ))
    }

    pub fn rate_text(&self) -> String {
        let r = &self.rate;
        let t = &r.throttle;
        format!(
            "観戦帯域の調整: 半減 {} 回 / 停止 {} 回 / 再開 {} 回 / 停止の合計 {} ms / RTT のサンプル {} / 現在の割合 {:.1}%{}",
            t.cuts,
            t.pauses,
            t.resumes,
            t.paused_ms,
            r.delays,
            f64::from(r.level_ppm) / 10_000.0,
            if r.paused { "（停止中）" } else { "" }
        )
    }

    pub fn text(&self) -> String {
        let h = &self.host;
        let r = &self.ring;
        let s = &self.send;
        let v = &self.via;
        let line = format!(
            "観戦: 観戦者 {} 人（うち IPv6 の経路 {} 人。入った {} / 上限で断った {} / 鍵の計算の上限で断った {} / 自分で抜けた {} / 応答が無く外した {} / リングバッファの溢れで全員外した {} / IPv4 と IPv6 の両方で入った同じ観戦者の返事の無い方を外した {}） / 積んだ記録 {}（ゲームのリングバッファで捨てた {}・欠け {} 回） / 送った記録 {}（{} 通・送り直し {} 通・送り直した記録 {}（うち後ろが届いて時計を待たずに {}）・生きている合図 {}・リングバッファから落ちて飛ばした {}・追いつきの送りを上限で次の周回へ回した {}） / 受けた ACK {}（積んだより先の嘘 {}・受け取れた範囲の嘘 {}） / 捨てた受信（HGRT でない {}・別の session {}・タグ {}・読めない {}・バージョン違い {}・受けない type {}・知らない観戦者 {}） / 受けた観戦のデータグラム（IPv4 {}・IPv6 {}。錠待ちの列が溢れて捨てた {}） / 送り口 {} 通（うち IPv6 {}・ガードで止めた {}・誤り {}）",
            self.spectators,
            self.spectators_v6,
            h.joined,
            h.refused_full,
            h.join_throttled,
            h.left,
            h.silenced,
            h.overflows,
            h.twins_dropped,
            h.pushed,
            r.dropped_full + r.dropped_gap,
            r.gaps,
            h.records_sent,
            h.packets_sent,
            h.resends,
            h.records_resent,
            h.records_fast,
            h.heartbeats,
            h.jumps,
            h.deferred,
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
            v.recv_v4,
            v.recv_v6,
            v.queue_dropped,
            s.sent,
            s.sent_v6,
            s.refused,
            s.errors,
        );
        // 配る側の用意が無い実行（ini の `[spectate]` の LAN / loopback の経路）は、今までと同じ行のまま。
        let offer = if self.offer.armed {
            let p = &self.punch;
            format!(
                "{} / 穴あけ（予定作成 {}・送信 {} 通・観戦者の JOIN で停止 {}・送信途中で押し出し {}）",
                self.offer.text(),
                p.started,
                p.sent,
                p.stopped,
                p.evicted
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

/// `Snapshot` の保存先（受信スレッドが書き、読み取りのスレッドが読む。錠は数百バイトのコピーの間だけ）と、配る側のスレッドから
/// 受信スレッドへの穴あけの頼み。
#[derive(Debug, Default)]
pub struct Shared {
    snap: Mutex<Snapshot>,
    /// 穴あけの頼み（配る側のスレッドが書き、受信スレッドが取る。受信スレッドが取るまでの分を古い順に持つ）。錠は書く・取る間だけ。
    punch: Mutex<PunchInbox>,
    /// 頼みの世代。配る側が頼みを書いた後に進め、受信スレッドは周回ごとにこの原子変数 1 つだけを見る（錠は世代が変わったときだけ）。
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
        self.punch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(candidates);
        // 値を書いた後に進める（読み手は世代が変わったのを見てから錠を取る）。
        self.punch_gen.fetch_add(1, Ordering::Release);
    }

    /// 受信スレッド: `seen` より新しい頼みがあれば古い順に `f` へ渡す（錠が取れなければ次の周回。待たない）。
    fn take_punch(&self, seen: &mut u32, f: impl FnMut(Candidates)) {
        let generation = self.punch_gen.load(Ordering::Acquire);
        if generation == *seen {
            return;
        }
        let mut g = match self.punch.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        *seen = generation;
        g.drain(f);
    }

    /// 検査だけ: 観戦者の数を設定する（観戦のホストの代わり）。
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

    /// 受信スレッドがまだ取っていない頼みのうち、いちばん新しいもの。
    #[cfg(test)]
    pub fn peek_punch(&self) -> Option<Candidates> {
        self.punch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .items
            .iter()
            .rev()
            .flatten()
            .next()
            .copied()
    }
}

/// 穴あけの予定の数（観戦者の上限と同じ。観戦者ごとに 1 つ）。
pub const PUNCH_PLANS: usize = MAX_SPECTATORS;

/// 受信スレッドが取るまでの穴あけの頼み。固定長で、頼みが `PUNCH_PLANS` 件を越えて溜まったら古い方を捨てる（受信スレッドは
/// 周回ごとに取るので、観戦者が一斉に返しても越えない）。
#[derive(Debug, Default)]
struct PunchInbox {
    items: [Option<Candidates>; PUNCH_PLANS],
}

impl PunchInbox {
    fn push(&mut self, c: Candidates) {
        if let Some(free) = self.items.iter_mut().find(|s| s.is_none()) {
            *free = Some(c);
            return;
        }
        self.items.rotate_left(1);
        if let Some(last) = self.items.last_mut() {
            *last = Some(c);
        }
    }

    fn drain(&mut self, mut f: impl FnMut(Candidates)) {
        for s in &mut self.items {
            if let Some(c) = s.take() {
                f(c);
            }
        }
    }
}

/// 観戦者 1 人ぶんの穴あけの予定と、頼まれた候補（同じ観戦者の頼み直しを見分けるため）。
struct PunchSlot {
    plan: PunchPlan,
    cands: Candidates,
}

impl PunchSlot {
    fn none() -> Self {
        Self {
            plan: PunchPlan::none(),
            cands: Candidates::EMPTY,
        }
    }

    /// 候補の集合が全部同じ（順は問わない）か。同じ公開 IPv4 の後ろの別の観戦者は、公開側の候補だけが重なる。
    fn same_candidates(&self, c: &Candidates) -> bool {
        let (a, b) = (self.cands.as_slice(), c.as_slice());
        let has = |list: &[Candidate], x: SocketAddr| list.iter().any(|y| same_endpoint(x, y.addr));
        !a.is_empty() && a.iter().all(|x| has(b, x.addr)) && b.iter().all(|y| has(a, y.addr))
    }
}

/// 観戦者ごとの穴あけの予定。頼みは、同じ観戦者（候補の集合が同じ）の予定があればそれを作り直し、無ければ送り終えた枠か空きの枠、
/// それも無ければいちばん先まで送った予定を押し出す。
struct PunchPlans {
    slots: [PunchSlot; PUNCH_PLANS],
}

impl PunchPlans {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| PunchSlot::none()),
        }
    }

    /// 予定を作る。押し出したら true。
    fn adopt(&mut self, c: Candidates, now: u32) -> bool {
        let same = self.slots.iter().position(|s| s.same_candidates(&c));
        let free = || self.slots.iter().position(|s| s.plan.is_done());
        let oldest = || {
            self.slots
                .iter()
                .enumerate()
                .max_by_key(|(i, s)| (s.plan.rounds_sent(), std::cmp::Reverse(*i)))
                .map(|(i, _)| i)
        };
        let (i, evicted) = match same.or_else(free) {
            Some(i) => (i, false),
            None => (oldest().unwrap_or(0), true),
        };
        if let Some(s) = self.slots.get_mut(i) {
            *s = PunchSlot {
                plan: PunchPlan::new(&c, now),
                cands: c,
            };
        }
        evicted
    }

    /// 全部の予定を進める。送った通数。
    fn poll<O: Outlet>(&mut self, now: u32, out: &mut O) -> usize {
        self.slots.iter_mut().map(|s| s.plan.poll(now, out)).sum()
    }

    /// `from` からタグの合う JOIN が来た: そのアドレスへの穴あけを止めた予定の数。
    fn stop_for(&mut self, from: SocketAddr) -> u32 {
        let mut n = 0;
        for s in &mut self.slots {
            if s.plan.stop_for(from) {
                n += 1;
            }
        }
        n
    }
}

/// 受信スレッドが観戦で使う送信ソケット。宛先の族で選ぶ（`GameNet::send` と同じ。IPv6 のソケットを開いていなければ `primary` のまま）。
/// IPv4 と IPv6 のどちらの受信スレッドから呼ばれても同じ 2 本を渡す。
#[derive(Clone, Copy)]
pub struct Socks<'a> {
    pub primary: &'a UdpSocket,
    pub v6: Option<&'a UdpSocket>,
}

/// IPv6 のソケットを持たない形（検査と、IPv4 だけの実行）。
impl<'a> From<&'a UdpSocket> for Socks<'a> {
    fn from(primary: &'a UdpSocket) -> Self {
        Socks { primary, v6: None }
    }
}

/// 受信スレッドの送信ソケット。宛先がネットワークの範囲の中かを送る直前に見る（`GameNet::may_send` と同じ関数・同じ範囲）。
struct SockOut<'a> {
    socks: Socks<'a>,
    scope: Scope,
    counts: &'a mut SendCounts,
}

impl Outlet for SockOut<'_> {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        if !guard::allows(self.scope, to) {
            self.counts.refused = self.counts.refused.saturating_add(1);
            return;
        }
        let sock = match (to.is_ipv6(), self.socks.v6) {
            (true, Some(s6)) => s6,
            _ => self.socks.primary,
        };
        match sock.send_to(bytes, to) {
            Ok(_) => {
                self.counts.sent = self.counts.sent.saturating_add(1);
                if to.is_ipv6() {
                    self.counts.sent_v6 = self.counts.sent_v6.saturating_add(1);
                }
            }
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
    via: ViaCounts,
    /// 観戦者の NAT へ穴をあける予定（配る側の頼みごとに観戦者 1 人分を作る。頼みが無い間は何も送らない）。
    punch: PunchPlans,
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
            via: ViaCounts::default(),
            punch: PunchPlans::new(),
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

    /// 対戦の往復の要約（ゲームのスレッドが測った値。`net_udp` が `tick` の前に渡す）。
    pub fn note_delay(&mut self, snap: Option<higanr_core::rtt_probe::DelaySnapshot>) {
        self.host.note_delay(snap);
    }

    /// 全体の上限に今掛けている割合（ppm）と、追いついた観戦者への送りも止めているか（検査用）。
    #[cfg(test)]
    pub fn throttle_state(&self) -> (u32, bool) {
        self.host.throttle_state()
    }

    /// 検査と実機のテスト用: 上りの測りで 2 組以上そろった番の見積もり（B/s）を、この値にする（片方を太く見せる）。ini の
    /// `[spectate] fake_uplink`。
    pub fn set_fake_uplink(&mut self, bps: Option<u32>) {
        self.host.set_fake_uplink(bps);
    }

    /// 観戦のホストが使っている `fake_uplink`（ini から本物のホストまで届いたかを見る検査用）。
    #[cfg(test)]
    pub fn fake_uplink(&self) -> Option<u32> {
        self.host.fake_uplink()
    }

    /// 受信スレッドが読んだ 1 通（`t_us` は読んだ時刻。µs）。測りの組 PAIR は到着の時刻のまま `SpectateHost::on_pair` へ、ほかは
    /// ms の時刻で `on_datagram` へ。集計は書かない（続けて届く便のたびに数え直さない。次の `tick` が 1 回書く）。
    pub fn on_recv<'a>(
        &mut self,
        socks: impl Into<Socks<'a>>,
        from: SocketAddr,
        bytes: &[u8],
        t_us: u64,
    ) {
        let now = (t_us / 1_000) as u32;
        self.dispatch(socks.into(), from, bytes, now, Some(t_us));
    }

    /// ms の時刻で渡す入口（測りの組は届いても `Unexpected` で捨てる。製品は `on_recv`）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn on_datagram<'a>(
        &mut self,
        socks: impl Into<Socks<'a>>,
        from: SocketAddr,
        bytes: &[u8],
        now: u32,
    ) {
        self.dispatch(socks.into(), from, bytes, now, None);
        self.publish();
    }

    fn dispatch(
        &mut self,
        socks: Socks<'_>,
        from: SocketAddr,
        bytes: &[u8],
        now: u32,
        t_us: Option<u64>,
    ) {
        #[cfg(test)]
        let began = std::time::Instant::now();
        let recv = if from.is_ipv6() {
            &mut self.via.recv_v6
        } else {
            &mut self.via.recv_v4
        };
        *recv = recv.saturating_add(1);
        if let Some(t_us) = t_us
            && wire::peek_type(bytes) == PeekedType::SpectatePair
        {
            self.host.on_pair(from, bytes, t_us);
        } else {
            let mut out = SockOut {
                socks,
                scope: self.scope,
                counts: &mut self.send,
            };
            let ev = self.host.on_datagram(from, bytes, now, &mut out);
            // タグの合う JOIN が来た = その観戦者までの穴が開いた。そのアドレスへの穴あけは止める（残りの候補へは続ける）。ホストが紹介した
            // 観戦者（中継役の下へ回す）も、ホストへの JOIN が通ったので同じ。
            if matches!(
                ev,
                HostEvent::Joined | HostEvent::Refreshed | HostEvent::Introduced
            ) {
                let n = self.punch.stop_for(from);
                self.punch_counts.stopped = self.punch_counts.stopped.saturating_add(n);
            }
        }
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
    pub fn tick<'a>(&mut self, socks: impl Into<Socks<'a>>, now: u32) {
        let socks = socks.into();
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
        let (plans, counts) = (&mut self.punch, &mut self.punch_counts);
        self.shared.take_punch(&mut self.punch_seen, |c| {
            if plans.adopt(c, now) {
                counts.evicted = counts.evicted.saturating_add(1);
            }
            counts.started = counts.started.saturating_add(1);
        });
        let mut out = SockOut {
            socks,
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
            spectators_v6: self.host.spectators().filter(|(a, ..)| a.is_ipv6()).count() as u32,
            relays: self
                .host
                .spectators()
                .filter(|(a, ..)| self.host.is_relay_of(*a))
                .count() as u32,
            ring: self.ring.counts(),
            send: self.send,
            via: self.via,
            punch: self.punch_counts,
            // 配る側の数は `Shared::get` が原子変数から足す（配る側のスレッドが書くので、ここでコピーしない）。
            offer: OfferCounts::default(),
            stopped: false,
            rate: {
                let (level_ppm, paused) = self.host.throttle_state();
                RateSnap {
                    throttle: self.host.throttle_counts(),
                    level_ppm,
                    paused,
                    delays: self.host.delay_seen(),
                }
            },
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
    Err("観戦用の session を対戦の session と別の値に決められません".to_string())
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
    .map_err(|e| format!("観戦用のコードを書けません: {e:?}"))?;
    let ring = Arc::new(SpectateRing::new());
    let mut sender = SpectateSender::new(
        host,
        Arc::clone(&ring),
        s.scope,
        Arc::new(Shared::default()),
    );
    sender.set_fake_uplink(s.spectate.as_ref().and_then(|x| x.fake_uplink));
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
                return Err("観戦用の session を OS の乱数で決められません".to_string());
            }
            Ok(u32::from_le_bytes(b))
        },
        crate::net_udp::draw_secret,
    )
}

pub fn write_code_file(dir: &Path, code_text: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{} の作成不能: {e}", dir.display()))?;
    crate::net_code::replace_file(dir, SPECTATE_CODE_FILE, &format!("{code_text}\r\n"))
}

pub struct Pending {
    sender: SpectateSender,
    code_text: String,
    dir: PathBuf,
    note: String,
}

/// 観戦を使う設定なら（`[spectate] enable = 1`）、リングバッファを `np` に付け、フォルダ `dir` と自分の受信ソケット `local` から観戦用のコードを用意する
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
            format!("{NOT_USED}コードの保存先か受信ソケットのアドレスが不明"),
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
pub(crate) const NOT_USED: &str = " / 観戦は使いません: ";
pub(crate) const NOT_INSTALLED: &str = " / 観戦の配信を開始できませんでした（送り手の登録が重複）";
const CODE_NOT_WRITTEN: &str = "。ただし、コードを書けませんでした: ";

/// ネットワークを開いた文に、観戦の準備の失敗の句が混ざっているか（成功の文は詳細だけ、失敗は簡潔にも書くため）。
pub fn note_has_problem(desc: &str) -> bool {
    [NOT_USED, NOT_INSTALLED, CODE_NOT_WRITTEN]
        .iter()
        .any(|m| desc.contains(m))
}

impl Pending {
    /// ネットワークのセッションをゲームへ渡せた後に、送り手を受信スレッドへ渡し、観戦用のコードを書く。戻りの文は本ログの 1 行に足す。
    /// コードを書けなくても送り手は渡す（対戦は止めない。観戦者が接続できないだけ）。
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
#[cfg(test)]
mod tree_tests;
