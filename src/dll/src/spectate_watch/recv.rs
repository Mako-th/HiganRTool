//! 観戦者の受け取り（受信スレッドで走る）。coreの`Watcher`を持ち、届いたパケットの復号・組み立て・親へのACK・JOINの再送をして、連続になった記録を`WatchFeed`へ移す。
//! ゲームのスレッドが止まっていても（ウィンドウのドラッグなど）受け続けるので、ホストの再送と沈黙による切り離しが増えない。
//! - `Watcher`と中継の配信側は観戦者のセッションを作るスレッドが`WatchSlot::offer`で作り（確保と鍵の計算）、ここが受け取る。接続する前なら新しい方へ替える。
//! - 中継の配信側があれば、受けた記録を番号のまま積んで子へ配信する（JOIN・ACKと子のBYEは配信側へ振り分ける）。
//! - BYEはゲームのスレッドの要求（`WatchFeed::end`）を見て親へ1通だけ出し（子がいればREHOMEも）、以後は何もしない。
//! - 照合のずれ（`WatchFeed::request_mismatch`）は、親が中継役でずれた記録がいまの親から来たときだけ元のホストへ直接戻る。
//! - 中継の配信側を持つ観戦者は接続した後に変わった候補（UPnP）とネットワーク種別のフラグを、知らせのJOINで元のホストへ渡す。
//! - 確保しない（替えた受け取りの解放だけ）。
#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use higanr_core::candidate::Candidates;
use higanr_core::link::Outlet;
use higanr_core::nat::NatMark;
use higanr_core::spectate::{JOIN_RESEND_MS, SpectateHost, Watcher, leave_tree, tend_relay};
use higanr_core::wire::{self, ByeReason, PeekedType};

use super::WatchSlot;
use super::feed::{RelayCounts, RelaySideCounts, WatchFeed, WatchUpnp};

fn ms(us: u64) -> u32 {
    (us / 1_000) as u32
}

/// 知らせのJOINを送る回数（`JOIN_RESEND_MS`おき）。1通届けばホストは読み直す。
pub const ANNOUNCE_ROUNDS: u8 = 4;

/// UPnPの結果（通信層が受信スレッドの周回ごとにポーリングし、出たときに1回渡す）。
// 1回渡すだけの値なので大きい方をBoxにしない（受信スレッドで確保しない）。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpnpNews {
    /// 頼み始めた。
    Asked,
    /// 割り当てて外側のアドレスがSTUNと合った。中身はUPnPの候補を足した自分の候補。
    Mapped(Candidates),
    /// 使わない（理由）。
    Unused(WatchUpnp),
}

/// 中継役のノードの受信の振り分け先。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Watcher,
    Relay,
    Drop,
}

/// JOIN・ACKと子からのBYEは配信側、PAIR（元のホストだけが受ける）は捨てる。ほかは観戦者の側。
fn route(relay: &SpectateHost, from: SocketAddr, bytes: &[u8]) -> Route {
    match wire::peek_type(bytes) {
        PeekedType::SpectateJoin | PeekedType::SpectateAck => Route::Relay,
        PeekedType::Bye if relay.ack_of(from).is_some() => Route::Relay,
        PeekedType::SpectatePair => Route::Drop,
        _ => Route::Watcher,
    }
}

pub struct WatchRecv {
    slot: Arc<WatchSlot>,
    feed: Arc<WatchFeed>,
    seen_gen: u32,
    watcher: Option<Box<Watcher>>,
    /// 中継の配信側（中継する設定で作れたときだけ。`watcher`と一緒に替える）。
    relay: Option<Box<SpectateHost>>,
    /// 自分のネットワーク種別のフラグ（受信スレッドが周回ごとに渡す）。中継の配信側を持つ観戦者のJOINにだけ載せる。
    nat: NatMark,
    upnp: WatchUpnp,
    /// UPnPの割り当てがSTUNと合って、まだ取り下げていない。
    upnp_held: bool,
    /// 中継の配信側が無いので割り当ての取り下げを通信層へ頼む（`take_upnp_drop`）。
    upnp_drop: bool,
    /// 候補かネットワーク種別のフラグが変わったので接続した後に知らせる。
    announce_due: bool,
    /// 知らせのJOINの残りの回数と次に送る時刻（ms）。
    announce_left: u8,
    announce_at: u32,
    announces_sent: u64,
    announces_skipped: u64,
    /// いまの親から来た記録がリングに積まれ始める位置。親を替えたときの、書いた数 ＋ 組み立てが持つ記録の数。
    /// 連続の後ろの飛び飛びの記録まで数える。
    parent_from: u32,
    /// `parent_from`を決めたときのepoch。
    seen_epoch: u16,
    mismatch_stays: u64,
    ff_max: u32,
    seen_jumps: u64,
    /// 次に移す記録が、流れが飛んだ後の最初の記録かどうか。
    jump: bool,
    /// BYEを出し終えた（ゲームのスレッドが観戦を終えた）。
    done: bool,
    /// 検査だけ: 次の`service`でパニックする。
    #[cfg(test)]
    pub(crate) panic_on_service: Arc<AtomicBool>,
}

impl WatchRecv {
    pub fn new(slot: Arc<WatchSlot>, feed: Arc<WatchFeed>, ff_max: u32) -> Self {
        Self {
            slot,
            feed,
            seen_gen: 0,
            watcher: None,
            relay: None,
            nat: NatMark::UNKNOWN,
            upnp: WatchUpnp::NotAsked,
            upnp_held: false,
            upnp_drop: false,
            announce_due: false,
            announce_left: 0,
            announce_at: 0,
            announces_sent: 0,
            announces_skipped: 0,
            parent_from: 0,
            seen_epoch: 0,
            mismatch_stays: 0,
            ff_max,
            seen_jumps: 0,
            jump: false,
            done: false,
            #[cfg(test)]
            panic_on_service: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn feed(&self) -> &Arc<WatchFeed> {
        &self.feed
    }

    fn adopt(&mut self, now_ms: u32) {
        if self.watcher.as_ref().is_some_and(|w| w.is_joined()) {
            return;
        }
        if let Some(o) = self.slot.take_newer(&mut self.seen_gen) {
            let mut w = o.watcher;
            // ホストはJOINの早送りの速さでこの観戦者への追いつきの送信の上限を決める。
            w.set_ff_max(self.ff_max);
            if o.relay.is_some() {
                w.set_nat(self.nat);
                if let Some(own) = self.slot.upnp_own() {
                    w.set_candidates(own);
                }
            }
            self.seen_epoch = w.epoch();
            // 新しい受け取りのJOINが今の候補とネットワーク種別のフラグを載せる。
            self.announce_due = false;
            self.watcher = Some(w);
            self.relay = o.relay;
            self.seen_jumps = 0;
            // 置き換える前の受け取りが立てたフラグが`offer`の後に残らないように。
            self.slot.clear_full_before_join();
            self.feed.set_code_at(now_ms);
        }
    }

    /// 自分のネットワーク種別のフラグ。2か所目のSTUNの結果はネットワークを開いた後に出るので周回ごとに受ける。
    pub fn note_nat(&mut self, nat: NatMark) {
        let changed = nat != self.nat;
        self.nat = nat;
        if self.relay.is_some()
            && let Some(w) = self.watcher.as_mut()
        {
            w.set_nat(nat);
            self.announce_due |= changed;
        }
    }

    /// UPnPの結果。割り当てが合ったら中継の配信側を持つ観戦者のJOINにUPnPの候補を載せる。
    pub fn note_upnp(&mut self, news: UpnpNews) {
        match news {
            UpnpNews::Asked => self.upnp = WatchUpnp::Waiting,
            UpnpNews::Mapped(own) => {
                self.upnp_held = true;
                self.slot.set_upnp_own(own);
                if self.relay.is_some()
                    && let Some(w) = self.watcher.as_mut()
                {
                    w.set_candidates(own);
                    self.announce_due = true;
                }
                self.settle_upnp();
            }
            UpnpNews::Unused(why) => self.upnp = why,
        }
    }

    /// 割り当てを使うかどうか。中継の配信側があれば使う。配信側の無い受け取りが接続したら（以後は替わらない）取り下げる。
    /// 接続する前は配信側のある受け取りへ替わるかもしれないので待つ。
    fn settle_upnp(&mut self) {
        if !self.upnp_held {
            return;
        }
        let joined = self.watcher.as_deref().is_some_and(Watcher::is_joined);
        self.upnp = if self.relay.is_some() {
            WatchUpnp::Mapped
        } else if joined {
            self.upnp_held = false;
            self.upnp_drop = true;
            WatchUpnp::NoRelay
        } else {
            WatchUpnp::Waiting
        };
    }

    /// 割り当ての取り下げの頼み（1回だけtrue）。通信層が`upnp::Handle::discard`で消す。
    pub fn take_upnp_drop(&mut self) -> bool {
        std::mem::take(&mut self.upnp_drop)
    }

    /// 受けた1通。記録を移したらtrue（呼び手がゲームのスレッドの待ちを起こす）。
    pub fn on_datagram<O: Outlet>(
        &mut self,
        from: SocketAddr,
        bytes: &[u8],
        t_us: u64,
        out: &mut O,
    ) -> bool {
        if self.done || self.feed.end_request().is_some() {
            return false;
        }
        let now = ms(t_us);
        self.adopt(now);
        let Some(w) = self.watcher.as_mut() else {
            return false;
        };
        let to = self
            .relay
            .as_deref()
            .map_or(Route::Watcher, |r| route(r, from, bytes));
        match (to, self.relay.as_mut()) {
            (Route::Relay, Some(r)) => {
                r.on_datagram(from, bytes, now, out);
            }
            (Route::Drop, _) => {}
            _ => w.on_datagram(from, bytes, now, out),
        }
        self.note_parent();
        self.settle(t_us)
    }

    /// 受信スレッドの周回ごとの処理: 新しい受け取りの受け取り・JOINの再送とACK・中継の配信側から子への送信・BYEの要求。
    pub fn service<O: Outlet>(&mut self, now_us: u64, out: &mut O) {
        #[cfg(test)]
        if self.panic_on_service.load(Ordering::Relaxed) {
            panic!("観戦者の受け取りの検査用のパニック");
        }
        if self.done {
            return;
        }
        if let Some(bye) = self.feed.end_request() {
            self.done = true;
            self.leave(bye, out);
            return;
        }
        let now = ms(now_us);
        self.adopt(now);
        if let Some(w) = self.watcher.as_mut() {
            w.poll(now, out);
            self.note_parent();
            self.on_mismatch(now, out);
            self.settle(now_us);
            if let (Some(w), Some(r)) = (self.watcher.as_mut(), self.relay.as_mut()) {
                tend_relay(w, r, now, out);
                r.poll(now, out);
            }
            self.settle_upnp();
            self.tend_announce(now, out);
            self.publish_tree();
        }
    }

    /// 親を替えた（epochが進んだ）ら、いまの親から来る記録がリングに積まれ始める位置を保存する。
    /// 組み立ては持ち越すので、置き場の記録（ackより先の飛び飛びの記録も）は前の親から来たものになる。
    /// その間の抜けをいまの親が埋めた分も前の親の分に数える（疑わない側に寄せる）。
    fn note_parent(&mut self) {
        let Some(w) = self.watcher.as_deref() else {
            return;
        };
        if w.epoch() != self.seen_epoch {
            self.seen_epoch = w.epoch();
            let held = w
                .stored_end()
                .wrapping_sub(w.ack())
                .wrapping_add(w.pending() as u32);
            self.parent_from = self.feed.written_pos().wrapping_add(held);
        }
    }

    /// ゲームのスレッドが照合のずれを渡したときの処理。次が全部揃ったら直接だけで元のホストへ戻る。
    /// 親が中継役・ずれた記録がいまの親から来た・そのマッチでポーズが崩れていない。前の親が運んだ記録や手元の崩れは、いまの親を疑う理由にならない。
    fn on_mismatch<O: Outlet>(&mut self, now: u32, out: &mut O) {
        let Some((pos, broken)) = self.feed.take_mismatch() else {
            return;
        };
        let Some(w) = self.watcher.as_mut() else {
            return;
        };
        if w.parent_is_home() {
            return;
        }
        // 位置の差はリングの長さより十分小さいので折り返しても符号で前後が分かる。
        let fresh = pos.wrapping_sub(self.parent_from) as i32 >= 0;
        if fresh && !broken && w.return_on_mismatch(now, out) {
            self.note_parent();
        } else {
            self.mismatch_stays += 1;
        }
    }

    /// 候補かネットワーク種別のフラグが変わったら、接続した後に知らせのJOINを`JOIN_RESEND_MS`おきに`ANNOUNCE_ROUNDS`回送る。
    /// 接続する前の再送のJOINが今の値を載せていても送る（そのJOINが落ちてホストが前のJOINで受け入れていると、古い値のまま残る）。
    /// 中継役になった後は送らない（依頼を受けたことがある。子は依頼を通らないと入れない）。
    /// 割り当てで外へ出る送信元が変わる機器では、ホストが別の送信元のJOINを2つ目の経路と見てしまうため（推定）。
    fn tend_announce<O: Outlet>(&mut self, now: u32, out: &mut O) {
        if self.relay.is_none() {
            return;
        }
        let Some(w) = self.watcher.as_mut() else {
            return;
        };
        if self.announce_due && w.is_joined() {
            self.announce_due = false;
            self.announce_left = ANNOUNCE_ROUNDS;
            self.announce_at = now;
        }
        if self.announce_left == 0 || (now.wrapping_sub(self.announce_at) as i32) < 0 {
            return;
        }
        let relaying = w.tree_counts().admits_taken > 0;
        if relaying || !w.announce(out) {
            self.announce_left = 0;
            self.announces_skipped += 1;
            return;
        }
        self.announces_sent += 1;
        self.announce_left -= 1;
        self.announce_at = now.wrapping_add(JOIN_RESEND_MS);
    }

    /// 受信スレッドの観戦者の処理がパニックしたとき・受信スレッドが止まるとき: 中継の配信側があれば親へBYE、子へREHOMEを送る。
    /// 子が黙りを待たずに元のホストへ戻れるようにするため。中継の配信側が無ければ今までどおり何も送らない。
    pub fn abandon<O: Outlet>(&mut self, out: &mut O) {
        if self.done || self.relay.is_none() {
            return;
        }
        self.done = true;
        self.leave(None, out);
    }

    /// 観戦を終える。頼まれたBYEを親へ送る。中継の配信側があれば子へREHOMEを送る（ホストが中継役を先に外して、戻る子の枠を取り置くため）。
    fn leave<O: Outlet>(&mut self, bye: Option<ByeReason>, out: &mut O) {
        let Some(w) = self.watcher.as_deref_mut() else {
            return;
        };
        match (bye, self.relay.as_deref_mut()) {
            (Some(r), relay) => leave_tree(w, relay, r, out),
            // ホストが黙った・受け取りが止まったときも子は元のホストへ戻し、親には抜けたことを知らせる。
            (None, Some(relay)) => leave_tree(w, Some(relay), ByeReason::Cancelled, out),
            (None, None) => {}
        }
        self.publish_tree();
    }

    /// 接続・沈黙・集計を渡し、組み立てが連続にした記録をリングへ移す（中継の配信側にも積む）。移したらtrue。
    fn settle(&mut self, t_us: u64) -> bool {
        let Some(w) = self.watcher.as_mut() else {
            return false;
        };
        if !self.feed.is_joined()
            && let Some(a) = w.host_addr()
        {
            self.slot.set_via_v6(a.is_ipv6());
            self.feed.set_joined(a, ms(t_us));
            self.slot.mark_joined();
        }
        if !w.is_joined() && w.host_bye() == Some(ByeReason::Cancelled) {
            self.slot.mark_full_before_join();
        }
        self.feed.set_refused(w.refused_after_join());
        self.feed.set_heard(w.last_heard());
        let jumps = w.assembler_counts().jumps;
        if jumps != self.seen_jumps {
            self.seen_jumps = jumps;
            self.jump = true;
        }
        let mut moved = false;
        while self.feed.has_room() {
            let Some((idx, rec)) = w.pop_indexed() else {
                break;
            };
            if let Some(r) = self.relay.as_mut() {
                r.push_at(idx, rec);
            }
            self.feed.push(&rec, self.jump, t_us as u32);
            self.jump = false;
            moved = true;
        }
        if w.pending() > 0 {
            // 置き場が残ったまま枠が無い（約1時間ぶん消費されていない）。ゲームのスレッドが観戦をやめる。
            self.feed.mark_full();
        }
        self.feed.publish_counts(&w.counts(), &w.assembler_counts());
        moved
    }

    /// 中継の木の数を本ログ用に渡す。
    fn publish_tree(&self) {
        let Some(w) = self.watcher.as_deref() else {
            return;
        };
        let side = self.relay.as_deref().map(|r| {
            let c = r.counts();
            RelaySideCounts {
                children: r.spectator_count() as u64,
                pushed: c.pushed,
                relay_jumps: c.relay_jumps,
                joined: c.joined,
                left: c.left,
                silenced: c.silenced,
                not_admitted: c.not_admitted,
                packets_sent: c.packets_sent,
                records_sent: c.records_sent,
                resends: c.resends,
                rehomes_sent: c.rehomes_sent,
                punches_sent: c.punches_sent,
            }
        });
        self.feed.publish_relay(&RelayCounts {
            enabled: self.slot.relay_enabled(),
            failed: u64::from(self.slot.relay_failed()),
            epoch: u64::from(w.epoch()),
            parent_is_home: w.parent_is_home(),
            direct_only: w.direct_only(),
            refused: w.refused_after_join(),
            tree: w.tree_counts(),
            side,
            mismatch_stays: self.mismatch_stays,
            upnp: self.upnp,
            announces_sent: self.announces_sent,
            announces_skipped: self.announces_skipped,
        });
    }

    /// 検査だけ: 観戦者。
    #[cfg(test)]
    pub(crate) fn watcher_for_test(&self) -> Option<&Watcher> {
        self.watcher.as_deref()
    }

    /// 検査だけ: いまの親から来た記録が積まれ始める位置。
    #[cfg(test)]
    pub(crate) fn parent_from_for_test(&self) -> u32 {
        self.parent_from
    }
}

#[cfg(test)]
mod tests {
    use higanr_core::auth::Secret;
    use higanr_core::seal::{DirKeys, KeySeed, Sender, Seq};
    use higanr_core::spectate::RelayKey;
    use higanr_core::wire::{MAX_DATAGRAM, Message, Record, SpectatePair, SpectateStream};

    use super::*;

    const SESSION: u32 = 0x5EC7_0C01;
    const FROM: SocketAddr = SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        41_401,
    );

    fn sealed(msg: &Message) -> Vec<u8> {
        let keys = DirKeys::from_raw([0x51; 32], [0x51; 32]);
        let mut b = [0u8; MAX_DATAGRAM];
        let n = wire::seal(
            &keys.sealer(Sender::Spectator),
            &Seq::new(),
            SESSION,
            msg,
            &mut b,
        )
        .expect("封じられる");
        b[..n].to_vec()
    }

    #[test]
    fn a_pair_is_dropped_and_other_sealed_messages_from_strangers_go_to_the_watcher() {
        let relay = SpectateHost::relay_with_capacity(
            SESSION,
            Secret([0x3C; 16]),
            RelayKey::new(&KeySeed([7; 32])),
            64,
        );
        let pair = sealed(&Message::SpectatePair(SpectatePair {
            turn: 1,
            pair: 0,
            second: false,
        }));
        assert_eq!(route(&relay, FROM, &pair), Route::Drop);
        let stream = sealed(&Message::SpectateStream(SpectateStream {
            first: 0,
            reset: true,
            cookie: None,
            count: 0,
            records: [Record::FILLER; wire::MAX_STREAM_RECORDS],
        }));
        assert_eq!(route(&relay, FROM, &stream), Route::Watcher);
        let bye = sealed(&Message::Bye(ByeReason::Finished));
        assert_eq!(
            route(&relay, FROM, &bye),
            Route::Watcher,
            "子でない送り元の BYE は親の BYE"
        );
    }
}
