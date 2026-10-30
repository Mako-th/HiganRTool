//! 相手との1回の接続ごとの本ログの行（pure。unsafe無し）。
//!
//! - 接続したときの1行（`connect_line`）は選択した項目・自分の側・待った秒数。どちらがホストに決まったかの1行は`pick_line`。書くのは接続コードの監視の処理のスレッド（ゲームのスレッドではない）。
//! - 接続の間とその終わりの行（`LinkLog`）は観戦者の出入り・stallの内訳・RTTの中央値と最大。読み取りのスレッドが1秒ごとに`NetSnapshot`から作る。
//!   ゲームのスレッドは数をアトミック変数へ書くだけ。ここで確保するのは読み取りのスレッドの中だけ。
#![forbid(unsafe_code)]

use higanr_core::host_pick::{Basis, PickNote};
use higanr_core::nat::{NatMark, Symmetry};
use higanr_core::net_menu::{T_CREATE, T_GROUP_LAN, T_PASTE, T_RANDOM, T_ROOM};

use crate::net_config::CodeSide;
use crate::net_watch::{NetSnapshot, SpectatorCounts};
use crate::trial_summary::{CodeFacts, Route};

/// 項目の名前（メニューの文字そのもの）。ログと試験のまとめが同じ言葉を使う。
pub fn route_name(r: Route) -> &'static str {
    match r {
        Route::Random => T_RANDOM.ja,
        Route::Room => T_ROOM.ja,
        Route::Create => T_CREATE.ja,
        Route::Paste => T_PASTE.ja,
        Route::Lan => T_GROUP_LAN.ja,
    }
}

/// 項目ごとの自分の側。ランダムマッチは待ち列に入った順、ルームマッチはルームに入った順、コードは作った側（始める側）か貼った側（返す側）。
fn side_words(c: &CodeFacts, r: Route) -> &'static str {
    match (r, c.queue_first, c.placed_side) {
        (Route::Random, Some(true), _) => "待ち列に先に入った側",
        (Route::Random, Some(false), _) => "待ち列に後から入った側",
        (Route::Random, None, _) => "待ち列での順は不明",
        (Route::Room, _, Some(CodeSide::Offer)) => "ルームに先に入った側",
        (Route::Room, _, Some(CodeSide::Answer)) => "ルームに後から入った側",
        (Route::Room, _, _) => "ルームでの順は不明",
        (Route::Create, _, _) => "始める側",
        (Route::Paste, _, _) => "返す側",
        (Route::Lan, _, Some(CodeSide::Offer)) => "ホストを選んだ側",
        (Route::Lan, _, Some(CodeSide::Answer)) => "クライアントを選んだ側",
        (Route::Lan, _, _) => "LAN での側は不明",
    }
}

/// 「ランダムマッチ（待ち列に先に入った側）」の形。項目を選んでいない実行はNone。
pub fn route_label(c: &CodeFacts) -> Option<String> {
    let r = c.route?;
    Some(format!("{}（{}）", route_name(r), side_words(c, r)))
}

fn span(from: Option<f32>, to: Option<f32>) -> Option<f32> {
    let (a, b) = (from?, to?);
    (b >= a).then_some(b - a)
}

fn secs_phrase(what: &str, secs: Option<f32>) -> Option<String> {
    secs.map(|s| format!("{what} {s:.1} 秒"))
}

/// 相手のコードを受け取る（読む）までに待った秒数の句。まだ着いていない区間は出さない（0秒と読まれないように）。
pub fn waits_to_read(c: &CodeFacts) -> Vec<String> {
    let Some(r) = c.route else {
        return Vec::new();
    };
    let mut v = Vec::new();
    let mut add = |what: &str, secs: Option<f32>| v.extend(secs_phrase(what, secs));
    match r {
        Route::Random => {
            add("待ち列で組になるまで", span(c.chosen_at, c.paired_at));
            add(
                "割り当てのルームで相手のコードを受け取るまで",
                span(c.paired_at, c.peer_read_at),
            );
        }
        Route::Room => add(
            "ルームで相手のコードを受け取るまで",
            span(c.chosen_at, c.peer_read_at),
        ),
        Route::Create => add(
            "コードを作ってから相手の返答コードを読むまで",
            span(c.chosen_at, c.peer_read_at),
        ),
        Route::Paste => add(
            "コードを貼り始めてから読めるまで",
            span(c.chosen_at, c.peer_read_at),
        ),
        Route::Lan => add(
            "相手のコードを受け渡すまで",
            span(c.chosen_at, c.peer_read_at),
        ),
    }
    v
}

/// `waits_to_read`に、接続の手順を始めてから接続できるまでを足したもの。
pub fn waits(c: &CodeFacts) -> Vec<String> {
    let mut v = waits_to_read(c);
    if c.route.is_some() {
        v.extend(secs_phrase(
            "接続の手順を始めてから接続できるまで",
            span(c.peer_read_at, c.connected_at),
        ));
    }
    v
}

/// 接続した1行（見張りが接続を見た1回だけ）。
pub fn connect_line(c: &CodeFacts) -> String {
    let Some(r) = c.route else {
        return "メニューで選んだ項目は分からないまま、接続しました。".to_string();
    };
    let host = match c.host {
        Some(true) => "・対戦のホスト",
        Some(false) => "・対戦のゲスト",
        None => "",
    };
    let waits = waits(c);
    format!(
        "{}（{}{host}）で接続しました。{}",
        route_name(r),
        side_words(c, r),
        waits.join(" / ")
    )
}

fn up_words(bps: u32) -> String {
    if bps == 0 {
        "測定不能".to_string()
    } else {
        format!("{:.2} Mbps", f64::from(bps) * 8.0 / 1_000_000.0)
    }
}

fn nat_words(m: NatMark) -> String {
    let kind = match m.symmetry {
        Symmetry::Unknown => "不明",
        Symmetry::No => "非シンメトリック",
        Symmetry::Yes => "シンメトリック",
    };
    if m.upnp {
        format!("{kind}・UPnP")
    } else {
        kind.to_string()
    }
}

/// どちらがホストに決まったかの1行（接続した1回だけ）。`host`は自分がホストかどうか（分からなければNone）。
pub fn pick_line(p: &PickNote, host: Option<bool>) -> String {
    let who = match host {
        Some(true) => "自分",
        Some(false) => "相手",
        None => "不明",
    };
    if !p.active {
        return format!("対戦のホスト: {who}（理由: 乱数の比較。上り回線速度の測定は未実行）。");
    }
    let basis = match p.basis {
        Basis::Nat => "NAT の種類",
        Basis::Uplink => "上り回線速度",
        Basis::Nonce => "乱数",
    };
    format!(
        "対戦のホスト: {who}（理由: {basis}）。上り: 自分 {}／相手 {}。NAT: 自分 {}／相手 {}。",
        up_words(p.own_up),
        up_words(p.peer_up),
        nat_words(p.own_nat),
        nat_words(p.peer_nat)
    )
}

/// 接続できないまま待ちが終わった1行。`what`は終わり方（取りやめました・時間切れ・ネットワークが終わりました）。`now`は監視の処理を起動してからの秒。
pub fn unconnected_line(c: &CodeFacts, what: &str, now: f32) -> String {
    let Some(label) = route_label(c) else {
        return format!("{what}（メニューで選んだ項目は分からない）");
    };
    let waited = c
        .chosen_at
        .and_then(|a| (now >= a).then_some(now - a))
        .map_or(String::new(), |s| format!("を選んでから {s:.1} 秒"));
    let waits = waits(c);
    let tail = if waits.is_empty() {
        String::new()
    } else {
        format!("（{}）", waits.join(" / "))
    };
    format!("{what}。{label}{waited}{tail}")
}

// ---- 読み取りのスレッド（1秒ごと）----

/// RTTのサンプルのバケットの数（1 ms刻み。最後のバケットは「その値以上」）。
const RTT_BUCKETS: usize = 1024;

/// 接続の間のRTTのサンプル（1秒ごとに読んだ最後の測定値）。同じPONGを2度取り込まないようPONGの数で見る。
#[derive(Debug, Clone)]
struct RttTally {
    hist: Vec<u32>,
    n: u32,
    max: u32,
    /// 最後にサンプルにしたPONGの数と、サンプルにできずに見逃したPONGの数（1秒の間に2個以上届いたとき）。
    seen_pongs: u32,
    missed: u32,
}

impl RttTally {
    fn new() -> RttTally {
        RttTally {
            hist: vec![0; RTT_BUCKETS],
            n: 0,
            max: 0,
            seen_pongs: 0,
            missed: 0,
        }
    }

    fn feed(&mut self, rtt_ms: Option<u32>, pongs: u32) {
        let Some(rtt) = rtt_ms else {
            return;
        };
        if pongs == self.seen_pongs {
            return;
        }
        self.missed = self
            .missed
            .saturating_add(pongs.saturating_sub(self.seen_pongs).saturating_sub(1));
        self.seen_pongs = pongs;
        let slot = (rtt as usize).min(RTT_BUCKETS - 1);
        if let Some(b) = self.hist.get_mut(slot) {
            *b = b.saturating_add(1);
        }
        self.n = self.n.saturating_add(1);
        self.max = self.max.max(rtt);
    }

    /// 偶数個なら真ん中の2つの平均（切り捨て）。枠の上限を越える値は上限に丸める。
    fn median(&self) -> Option<u32> {
        if self.n == 0 {
            return None;
        }
        let nth = |k: u32| {
            let mut acc = 0u32;
            for (ms, c) in self.hist.iter().enumerate() {
                acc = acc.saturating_add(*c);
                if acc > k {
                    return ms as u32;
                }
            }
            (RTT_BUCKETS - 1) as u32
        };
        let (lo, hi) = (nth((self.n - 1) / 2), nth(self.n / 2));
        Some(((u64::from(lo) + u64::from(hi)) / 2) as u32)
    }
}

/// 読み取りのスレッドが持つ接続1本分の状態。接続が替わったら（`link_started_ms`が変わる）作り直す。
#[derive(Debug, Clone)]
pub struct LinkLog {
    link: u32,
    rtt: RttTally,
    spec: SpectatorCounts,
}

impl Default for LinkLog {
    fn default() -> Self {
        LinkLog {
            link: 0,
            rtt: RttTally::new(),
            spec: SpectatorCounts::default(),
        }
    }
}

impl LinkLog {
    /// 1秒ごとに呼ぶ。書く行を返す（観戦者が来た・抜けた・断った。無ければ空）。
    pub fn step(&mut self, snap: &NetSnapshot, k: u32) -> Vec<String> {
        if snap.link_started_ms != self.link {
            *self = LinkLog {
                link: snap.link_started_ms,
                ..LinkLog::default()
            };
        }
        self.rtt.feed(snap.rtt_ms, snap.rtt_pongs);
        let Some(now) = snap.spectators else {
            return Vec::new();
        };
        let before = std::mem::replace(&mut self.spec, now);
        let d = |a: u32, b: u32| a.saturating_sub(b);
        let mut lines = Vec::new();
        if now.joined > before.joined {
            lines.push(format!(
                "観戦: 自分に観戦者が入りました（{} 人。いま {} 人、{k} 秒の時点）。",
                d(now.joined, before.joined),
                now.now
            ));
        }
        let (left, silenced) = (d(now.left, before.left), d(now.silenced, before.silenced));
        if left + silenced > 0 {
            lines.push(format!(
                "観戦: 観戦者が抜けました（自分で抜けた {left} 人・応答が無く外した {silenced} 人。いま {} 人、{k} 秒の時点）。",
                now.now
            ));
        }
        if now.refused_full > before.refused_full {
            lines.push(format!(
                "観戦: 上限で観戦者を断りました（{} 人、{k} 秒の時点）。",
                d(now.refused_full, before.refused_full)
            ));
        }
        lines
    }

    /// 接続の終わりに足す行（stallの内訳・RTT・観戦者）。`snap`はこの接続の最後の数。
    pub fn end_lines(&self, snap: &NetSnapshot) -> Vec<String> {
        let mut v = Vec::new();
        if let Some(l) = stall_line(snap) {
            v.push(l);
        }
        if let Some(l) = self.rtt_line(snap) {
            v.push(l);
        }
        if let Some(l) = spectators_line(snap) {
            v.push(l);
        }
        v
    }

    fn rtt_line(&self, snap: &NetSnapshot) -> Option<String> {
        if snap.watcher || !snap.connected {
            return None;
        }
        let t = &self.rtt;
        let Some(median) = t.median() else {
            return Some(format!(
                "接続: RTT は測れていない（測れた PONG {} 個のうち、読み取りで採れた標本 0 個）",
                snap.rtt_pongs
            ));
        };
        Some(format!(
            "接続: RTT 中央値 {median} ms / 最大 {} ms（測れた PONG {} 個のうち、1 秒ごとの読み取りで採れた標本 {} 個。見逃し {} 個）",
            t.max, snap.rtt_pongs, t.n, t.missed
        ))
    }
}

/// stallの内訳。5つの和を「合計」に出す（`NetSnapshot::stalls`と同じ場所で数えた値）。
pub fn stall_line(s: &NetSnapshot) -> Option<String> {
    if s.watcher || !s.connected {
        return None;
    }
    let sum = u64::from(s.stalls_sync)
        + u64::from(s.stalls_ahead)
        + u64::from(s.stalls_boundary)
        + u64::from(s.stalls_other)
        + u64::from(s.stalls_peer);
    let rate = if s.played > 0 {
        format!(
            "。使ったフレーム {} のうち {:.2}%",
            s.played,
            sum as f64 * 100.0 / f64::from(s.played)
        )
    } else {
        String::new()
    };
    let note = if sum == u64::from(s.stalls) {
        String::new()
    } else {
        format!("（読み出しの途中で stall の数が {} になった）", s.stalls)
    };
    Some(format!(
        "接続: stall の内訳（フレーム数）: 時刻合わせ {} / 先に行き過ぎ {} / 境目 {} / 相手の入力待ち {} / ほか {}（合計 {sum}{note}）{rate}",
        s.stalls_sync, s.stalls_ahead, s.stalls_boundary, s.stalls_peer, s.stalls_other
    ))
}

fn spectators_line(s: &NetSnapshot) -> Option<String> {
    let c = s.spectators?;
    Some(format!(
        "接続: 観戦者 入った {} 人 / 自分で抜けた {} 人 / 応答が無く外した {} 人 / 上限で断った {} 人（終わりにいた {} 人）",
        c.joined, c.left, c.silenced, c.refused_full, c.now
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(route: Route) -> CodeFacts {
        CodeFacts {
            route: Some(route),
            chosen_at: Some(1.0),
            ..CodeFacts::new(CodeSide::Game)
        }
    }

    fn snap() -> NetSnapshot {
        NetSnapshot {
            connected: true,
            link_started_ms: 5_000,
            ..NetSnapshot::default()
        }
    }

    #[test]
    fn a_random_match_line_names_the_route_side_and_each_wait() {
        let c = CodeFacts {
            paired_at: Some(76.6),
            peer_read_at: Some(77.9),
            connected_at: Some(78.2),
            queue_first: Some(true),
            host: Some(true),
            placed_side: Some(CodeSide::Offer),
            ..facts(Route::Random)
        };
        assert_eq!(
            connect_line(&c),
            "ランダムマッチ（待ち列に先に入った側・対戦のホスト）で接続しました。待ち列で組になるまで 75.6 秒 / 割り当てのルームで相手のコードを受け取るまで 1.3 秒 / 接続の手順を始めてから接続できるまで 0.3 秒"
        );
    }

    #[test]
    fn each_route_says_its_own_side() {
        let mk = |route, side, first| CodeFacts {
            placed_side: side,
            queue_first: first,
            ..facts(route)
        };
        let label = |c: &CodeFacts| route_label(c).unwrap();
        assert_eq!(
            label(&mk(Route::Room, Some(CodeSide::Answer), None)),
            "ルームマッチ（ルームに後から入った側）"
        );
        assert_eq!(
            label(&mk(Route::Random, None, Some(false))),
            "ランダムマッチ（待ち列に後から入った側）"
        );
        assert_eq!(
            label(&mk(Route::Create, Some(CodeSide::Offer), None)),
            "対戦コード作成（始める側）"
        );
        assert_eq!(
            label(&mk(Route::Paste, Some(CodeSide::Answer), None)),
            "対戦コード入力（返す側）"
        );
        assert_eq!(
            label(&mk(Route::Lan, Some(CodeSide::Offer), None)),
            "LAN（ホストを選んだ側）"
        );
        assert_eq!(route_label(&CodeFacts::new(CodeSide::Game)), None);
    }

    #[test]
    fn waits_skip_sections_that_have_not_been_reached() {
        let c = CodeFacts {
            peer_read_at: Some(5.0),
            ..facts(Route::Room)
        };
        assert_eq!(waits(&c), ["ルームで相手のコードを受け取るまで 4.0 秒"]);
        let c = CodeFacts {
            peer_read_at: Some(0.5),
            ..facts(Route::Room)
        };
        assert!(waits(&c).is_empty(), "{:?}", waits(&c));
    }

    #[test]
    fn an_unconnected_line_says_how_it_ended_and_how_long() {
        let c = CodeFacts {
            paired_at: Some(31.0),
            queue_first: Some(false),
            ..facts(Route::Random)
        };
        let l = unconnected_line(&c, "待ちを取りやめました", 41.0);
        assert!(
            l.starts_with(
                "待ちを取りやめました。ランダムマッチ（待ち列に後から入った側）を選んでから 40.0 秒"
            ),
            "{l}"
        );
        assert!(l.contains("待ち列で組になるまで 30.0 秒"), "{l}");
        assert_eq!(
            unconnected_line(&CodeFacts::new(CodeSide::Game), "待ちを取りやめました", 3.0),
            "待ちを取りやめました（メニューで選んだ項目は分からない）"
        );
    }

    fn sampled(values: &[u32]) -> RttTally {
        let mut t = RttTally::new();
        for (i, v) in values.iter().enumerate() {
            t.feed(Some(*v), i as u32 + 1);
        }
        t
    }

    #[test]
    fn rtt_median_and_max_come_from_every_distinct_pong() {
        let t = sampled(&[20, 18, 105, 18]);
        assert_eq!((t.n, t.max, t.median()), (4, 105, Some(19)));
        let mut t = sampled(&[30, 40, 50]);
        t.feed(Some(999), 3);
        assert_eq!(t.n, 3, "同じ PONG の数なら標本にしない");
        t.feed(Some(60), 6);
        assert_eq!(
            (t.n, t.missed),
            (4, 2),
            "PONG が 3 個飛んだ分は見逃しに数える"
        );
        assert_eq!(t.median(), Some(45));
        assert_eq!(RttTally::new().median(), None);
        let mut big = RttTally::new();
        big.feed(Some(5_000), 1);
        assert_eq!((big.max, big.median()), (5_000, Some(1_023)));
    }

    /// 観戦者の実行に出ない・使ったフレームに対する割合の分母があることも見る。
    #[test]
    fn the_stall_split_adds_up_to_the_stall_count() {
        let s = NetSnapshot {
            stalls: 286,
            stalls_sync: 20,
            stalls_ahead: 237,
            stalls_boundary: 25,
            stalls_other: 1,
            stalls_peer: 3,
            played: 14_700,
            ..snap()
        };
        let l = stall_line(&s).unwrap();
        assert!(
            l.contains(
                "時刻合わせ 20 / 先に行き過ぎ 237 / 境目 25 / 相手の入力待ち 3 / ほか 1（合計 286）"
            ) && l.ends_with("使ったフレーム 14700 のうち 1.95%"),
            "{l}"
        );
        let off = NetSnapshot { stalls: 290, ..s };
        assert!(
            stall_line(&off)
                .unwrap()
                .contains("stall の数が 290 になった"),
            "{}",
            stall_line(&off).unwrap()
        );
        assert!(stall_line(&NetSnapshot { watcher: true, ..s }).is_none());
        assert!(
            stall_line(&NetSnapshot {
                connected: false,
                ..s
            })
            .is_none()
        );
    }

    #[test]
    fn end_lines_carry_rtt_with_its_denominator_and_never_invent_one() {
        let mut log = LinkLog::default();
        let mut s = snap();
        assert!(
            log.end_lines(&s)
                .iter()
                .any(|l| l.contains("RTT は測れていない"))
        );
        for (i, rtt) in [176u32, 186, 176, 170].into_iter().enumerate() {
            s.rtt_ms = Some(rtt);
            s.rtt_pongs = i as u32 + 1;
            log.step(&s, i as u32 + 1);
        }
        s.rtt_pongs = 5;
        let lines = log.end_lines(&s);
        let rtt = lines.iter().find(|l| l.contains("RTT 中央値")).unwrap();
        assert_eq!(
            rtt,
            "接続: RTT 中央値 176 ms / 最大 186 ms（測れた PONG 5 個のうち、1 秒ごとの読み取りで採れた標本 4 個。見逃し 0 個）"
        );
    }

    #[test]
    fn a_new_link_starts_the_tally_over() {
        let mut log = LinkLog::default();
        let mut s = snap();
        s.rtt_ms = Some(300);
        s.rtt_pongs = 1;
        log.step(&s, 1);
        let next = NetSnapshot {
            link_started_ms: 9_000,
            rtt_ms: Some(20),
            rtt_pongs: 1,
            ..snap()
        };
        log.step(&next, 2);
        let l = log.end_lines(&next);
        assert!(
            l.iter().any(|x| x.contains("中央値 20 ms / 最大 20 ms")),
            "{l:?}"
        );
    }

    #[test]
    fn spectator_arrivals_and_departures_are_logged_once_each() {
        let mut log = LinkLog::default();
        let mk = |j, l, sil, refused, now| NetSnapshot {
            spectators: Some(SpectatorCounts {
                now,
                joined: j,
                left: l,
                silenced: sil,
                refused_full: refused,
            }),
            ..snap()
        };
        assert!(
            log.step(&snap(), 1).is_empty(),
            "観戦の用意が無い実行は何も書かない"
        );
        assert!(log.step(&mk(0, 0, 0, 0, 0), 2).is_empty());
        let l = log.step(&mk(2, 0, 0, 0, 2), 3);
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(
            l[0].contains("自分に観戦者が入りました（2 人。いま 2 人"),
            "{l:?}"
        );
        assert!(
            log.step(&mk(2, 0, 0, 0, 2), 4).is_empty(),
            "同じ数は書かない"
        );
        let l = log.step(&mk(2, 1, 1, 1, 0), 5);
        assert_eq!(l.len(), 2, "{l:?}");
        assert!(
            l[0].contains("自分で抜けた 1 人・応答が無く外した 1 人。いま 0 人"),
            "{l:?}"
        );
        assert!(l[1].contains("上限で観戦者を断りました（1 人"), "{l:?}");
        let end = log.end_lines(&mk(2, 1, 1, 1, 0));
        assert!(
            end.iter().any(|x| x.contains(
                "観戦者 入った 2 人 / 自分で抜けた 1 人 / 応答が無く外した 1 人 / 上限で断った 1 人"
            )),
            "{end:?}"
        );
    }

    #[test]
    fn the_pick_line_names_the_host_the_basis_and_both_sides() {
        let p = PickNote {
            active: true,
            basis: Basis::Uplink,
            own_up: 100_000,
            peer_up: 0,
            own_nat: NatMark {
                symmetry: Symmetry::No,
                upnp: true,
            },
            peer_nat: NatMark::UNKNOWN,
        };
        assert_eq!(
            pick_line(&p, Some(false)),
            "対戦のホスト: 相手（理由: 上り回線速度）。上り: 自分 0.80 Mbps／相手 測定不能。NAT: 自分 非シンメトリック・UPnP／相手 不明。"
        );
        assert_eq!(
            pick_line(&PickNote::NONE, Some(true)),
            "対戦のホスト: 自分（理由: 乱数の比較。上り回線速度の測定は未実行）。"
        );
        let nat = PickNote {
            basis: Basis::Nat,
            peer_nat: NatMark {
                symmetry: Symmetry::Yes,
                upnp: false,
            },
            ..p
        };
        assert!(
            pick_line(&nat, None).contains("不明（理由: NAT の種類）")
                && pick_line(&nat, None).contains("相手 シンメトリック。"),
            "{}",
            pick_line(&nat, None)
        );
    }
}
