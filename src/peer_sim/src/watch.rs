//! 観戦者の役（`peer_sim --watch`）。
//!
//! 観戦用の接続コード（または対戦のホストの観戦のポートのアドレス＋session・秘密）でホストへ SPECTATE_JOIN を送り、
//! 流れてくる記録を core の `Watcher` で並べ、マッチの語を CSV に書く。対戦者の `_net_frames.csv` と
//! 突き合わせる（`compare_spectate`）。プロトコルの決まりは `higanr_core::spectate` にあり、ここは呼ぶだけ。
//!
//! - 終わり方: ホストの `Record::End` を受けたら `LINGER_MS` の間ホストの送り直しへ ACK を返し、BYE(終了) を
//!   1 通出して終える。JOIN を断られた・流れが `silent_end_ms` 途切れた・`max_ms` に達したときも終える。
//! - CSV は 2 本（回ごとに別名）: マッチの語 `seg,frame,p1,p2` と、SELECT・押下・MENU の語・seg の始まり・終わりを
//!   記録の番号つきで並べた `index,type,fields`。
//! - 観戦者の送り（JOIN・ACK・BYE）に `FaultConfig` を掛けられる。`drop_after` は「繋がった後に黙る観戦者」の検査用。

use std::fmt::Write as _;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use higanr_core::auth::Secret;
use higanr_core::candidate::Candidates;
use higanr_core::guard::{self, Scope};
use higanr_core::handshake::Name;
use higanr_core::spectate::{AsmCounts, Watcher, WatcherConfig, WatcherCounts};
use higanr_core::wire::{ByeReason, Record, Seat, Select, Step};

use crate::fault::{FaultConfig, FaultCounts};
use crate::sim::{At, SimError, Tx};

/// 終わりの記録の後、ホストの送り直しへ ACK を返し続ける長さ（ms）。最後の ACK が落ちた場合に備える。
pub const LINGER_MS: u32 = 300;

/// マッチの語の CSV の見出し（1P・2P は dll の `_net_frames.csv` と同じ 16 進 4 桁）。
pub const FRAMES_CSV_HEADER: &str = "seg,frame,p1,p2";
pub const EVENTS_CSV_HEADER: &str = "index,type,fields";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchConfig {
    pub host: Vec<SocketAddr>,
    pub session_id: u32,
    pub secret: Secret,
    /// ホストの観戦用の公開鍵（観戦用のコードから）。
    pub host_public: [u8; higanr_core::seal::PUBLIC_LEN],
    pub name: String,
    pub nonce: u64,
    pub fault: FaultConfig,
    /// 実行し始めてから最初の JOIN までの長さ（ms）。遅れて入る検査用。
    pub start_after_ms: u32,
    /// 流れを聞いた後、これだけ何も聞かなければ「ホストが黙った」で終える（ms）。
    pub silent_end_ms: u32,
    pub max_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEnd {
    HostEnded(ByeReason),
    /// 流れを聞いた後で、`silent_end_ms` の間何も聞かなかった。
    HostSilent,
    /// JOIN を断られた（観戦者が上限）。
    Refused(ByeReason),
    Deadline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WFrame {
    pub seg: u16,
    pub frame: u32,
    pub p1: u16,
    pub p2: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WEvent {
    Seg(u16),
    Select {
        seat: Seat,
        sel: Select,
    },
    Step(Step),
    Menu {
        p1: u16,
        p2: u16,
    },
    /// 席ごとの名前（ホストが各 seg の `Seg` の直後に積む。UTF-8）。
    Name {
        seat: Seat,
        name: Name,
    },
    End(ByeReason),
}

#[derive(Debug, Clone)]
pub struct WatchReport {
    pub end: WatchEnd,
    /// 最初の STREAM を受けたか（JOIN が通った）。
    pub joined: bool,
    pub host_addr: Option<SocketAddr>,
    pub frames: Vec<WFrame>,
    pub events: Vec<(u32, WEvent)>,
    /// 取り出した記録の数（`jumps` は `reset` で今の seg の始まりへ飛んだ回数）。
    pub records: u32,
    pub jumps: u32,
    /// 直前の `Seg` が無いまま届いた語の数（0 のはず。あれば seg 0 として記録し数える）。
    pub frames_without_seg: u32,
    pub counts: WatcherCounts,
    pub asm: AsmCounts,
    pub faults: FaultCounts,
    pub sends: u64,
    pub send_errors: u64,
    pub send_refused: u64,
    pub elapsed_ms: u32,
}

fn host_candidates(cfg: &WatchConfig, scope: Scope) -> Result<Candidates, SimError> {
    for &a in &cfg.host {
        guard::check_peer(a, scope).map_err(SimError::Refused)?;
    }
    Candidates::from_addrs(&cfg.host).map_err(|e| {
        SimError::Refused(format!("ホストの候補 {:?} が並べられない: {e:?}", cfg.host))
    })
}

/// 観戦者を 1 回走らせる。`sock` はネットワークの範囲の中に bind した UDP ソケット。
pub fn run(cfg: &WatchConfig, sock: UdpSocket, scope: Scope) -> Result<WatchReport, SimError> {
    let local = sock.local_addr()?;
    guard::check_bind(local, scope, "観戦者の受信ソケット").map_err(SimError::Refused)?;
    let host = host_candidates(cfg, scope)?;
    cfg.fault.validate().map_err(SimError::Refused)?;
    sock.set_nonblocking(true)?;
    let mut w = Watcher::new(WatcherConfig {
        session_id: cfg.session_id,
        secret: cfg.secret,
        nonce: cfg.nonce,
        name: Name::new(&cfg.name),
        host,
        host_public: cfg.host_public,
        key_seed: crate::keys::fresh_seed(),
    });
    let mut tx = Tx::new(sock, cfg.fault, scope);
    let t0 = Instant::now();
    let now_ms = |t0: Instant| (t0.elapsed().as_micros() / 1_000) as u32;
    let mut rep = WatchReport {
        end: WatchEnd::Deadline,
        joined: false,
        host_addr: None,
        frames: Vec::new(),
        events: Vec::new(),
        records: 0,
        jumps: 0,
        frames_without_seg: 0,
        counts: WatcherCounts::default(),
        asm: AsmCounts::default(),
        faults: FaultCounts::default(),
        sends: 0,
        send_errors: 0,
        send_refused: 0,
        elapsed_ms: 0,
    };
    let mut cur_seg: Option<u16> = None;
    let mut last_index: Option<u32> = None;
    let mut ended: Option<(u32, ByeReason)> = None;
    let mut buf = [0u8; 2048];
    let end = loop {
        let now = now_ms(t0);
        if now >= cfg.max_ms {
            break WatchEnd::Deadline;
        }
        loop {
            match tx.sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let bytes = buf.get(..n).unwrap_or(&[]);
                    w.on_datagram(from, bytes, now, &mut At { tx: &mut tx, now });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::ConnectionReset => continue,
                Err(e) => return Err(SimError::Io(e)),
            }
        }
        if now >= cfg.start_after_ms {
            w.poll(now, &mut At { tx: &mut tx, now });
        }
        while let Some((idx, r)) = w.pop_indexed() {
            rep.records += 1;
            if last_index.is_some_and(|l| idx != l + 1) {
                rep.jumps += 1;
            }
            last_index = Some(idx);
            match r {
                Record::Seg { seg } => {
                    cur_seg = Some(seg);
                    rep.events.push((idx, WEvent::Seg(seg)));
                }
                Record::Frame { frame, p1, p2 } => {
                    if cur_seg.is_none() {
                        rep.frames_without_seg += 1;
                    }
                    rep.frames.push(WFrame {
                        seg: cur_seg.unwrap_or(0),
                        frame,
                        p1,
                        p2,
                    });
                }
                Record::Select { seat, sel } => {
                    rep.events.push((idx, WEvent::Select { seat, sel }))
                }
                Record::Step(s) => rep.events.push((idx, WEvent::Step(s))),
                Record::Menu { p1, p2 } => rep.events.push((idx, WEvent::Menu { p1, p2 })),
                Record::Name { seat, len, bytes } => {
                    // 読めなければ空の名前（パニックしない）。
                    let text = bytes
                        .get(..usize::from(len))
                        .and_then(|b| std::str::from_utf8(b).ok())
                        .unwrap_or("");
                    rep.events.push((
                        idx,
                        WEvent::Name {
                            seat,
                            name: Name::new(text),
                        },
                    ));
                }
                // 対戦の状態は画面の行に出すだけの値で、突き合わせ用の CSV には入れない（ホストが 1 秒ごとに積むため）。
                Record::Stats { .. } => {}
                Record::End(reason) => {
                    rep.events.push((idx, WEvent::End(reason)));
                    if ended.is_none() {
                        ended = Some((now, reason));
                    }
                }
            }
        }
        if let Some((at, reason)) = ended
            && now.wrapping_sub(at) >= LINGER_MS
        {
            break WatchEnd::HostEnded(reason);
        }
        if !w.is_joined()
            && let Some(r) = w.host_bye()
        {
            break WatchEnd::Refused(r);
        }
        if ended.is_none() && w.silent_ms(now).is_some_and(|s| s >= cfg.silent_end_ms) {
            break WatchEnd::HostSilent;
        }
        tx.flush(now);
        std::thread::sleep(Duration::from_millis(1));
    };
    let now = now_ms(t0);
    // 抜けるときは BYE(終了) を 1 通。落ちればホストは黙った観戦者として外す。
    if w.host_addr().is_some() {
        w.bye(ByeReason::Finished, &mut At { tx: &mut tx, now });
        tx.flush(now.wrapping_add(u32::MAX / 2));
    }
    rep.end = end;
    rep.joined = w.is_joined();
    rep.host_addr = w.host_addr();
    rep.counts = w.counts();
    rep.asm = w.assembler_counts();
    rep.faults = tx.faults.counts();
    rep.sends = tx.sends;
    rep.send_errors = tx.send_errors;
    rep.send_refused = tx.send_refused;
    rep.elapsed_ms = now;
    Ok(rep)
}

pub fn frames_csv(rep: &WatchReport) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{FRAMES_CSV_HEADER}");
    for f in &rep.frames {
        let _ = writeln!(s, "{},{},{:04X},{:04X}", f.seg, f.frame, f.p1, f.p2);
    }
    s
}

fn event_type(e: &WEvent) -> &'static str {
    match e {
        WEvent::Seg(_) => "seg",
        WEvent::Select { .. } => "select",
        WEvent::Step(_) => "step",
        WEvent::Menu { .. } => "menu",
        WEvent::Name { .. } => "name",
        WEvent::End(_) => "end",
    }
}

/// 押下 1 つの文字列の形（`record::log_text` の `# 押下` の行と同じ。突き合わせが文字で比べる）。
pub fn step_fields(st: &Step) -> String {
    format!(
        "step={} kind={:?} value={:#04X} screen={}",
        st.step, st.kind, st.value, st.screen
    )
}

fn event_fields(e: &WEvent) -> String {
    match e {
        WEvent::Seg(seg) => format!("seg={seg}"),
        WEvent::Select { seat, sel } => format!(
            "seat={} match_no={} chara={} color={} life={} charge={} map={} difficulty={} cpu_1p={} cpu_2p={} background={} rng_seed={} option_digest={}",
            match seat {
                Seat::P1 => 1,
                Seat::P2 => 2,
            },
            sel.match_no,
            sel.chara,
            sel.color,
            sel.life,
            sel.charge,
            sel.map,
            sel.difficulty,
            sel.cpu_1p,
            sel.cpu_2p,
            sel.background,
            sel.rng_seed,
            sel.option_digest
        ),
        WEvent::Step(st) => step_fields(st),
        WEvent::Menu { p1, p2 } => format!("p1={p1:04X} p2={p2:04X}"),
        WEvent::Name { seat, name } => format!(
            "seat={} name={:?}",
            match seat {
                Seat::P1 => 1,
                Seat::P2 => 2,
            },
            name.as_str()
        ),
        WEvent::End(r) => format!("reason={r:?}"),
    }
}

/// 記録の CSV（マッチの語は入れない。`frames_csv`）。
pub fn events_csv(rep: &WatchReport) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{EVENTS_CSV_HEADER}");
    for (idx, e) in &rep.events {
        let _ = writeln!(s, "{idx},{},{}", event_type(e), event_fields(e));
    }
    s
}

pub fn summary(rep: &WatchReport) -> String {
    let mut s = String::new();
    let end = match rep.end {
        WatchEnd::HostEnded(r) => format!("ホストが終わりを流した（{r:?}）"),
        WatchEnd::HostSilent => "流れを聞いた後でホストが黙った".to_string(),
        WatchEnd::Refused(r) => format!("JOIN を断られた（ホストの BYE: {r:?}）"),
        WatchEnd::Deadline => "打ち切りの時刻を過ぎた（--max-sec）".to_string(),
    };
    let _ = writeln!(s, "終わり方: {end}");
    let _ = writeln!(
        s,
        "JOIN: {} / ホスト {:?} / 経過 {} ms",
        if rep.joined {
            "通った"
        } else {
            "通らなかった"
        },
        rep.host_addr,
        rep.elapsed_ms
    );
    let _ = writeln!(
        s,
        "記録 {} 個（試合の語 {} フレーム・それ以外 {}）/ 番号が飛んだ {} 回 / seg の無い語 {}",
        rep.records,
        rep.frames.len(),
        rep.events.len(),
        rep.jumps,
        rep.frames_without_seg
    );
    let _ = writeln!(s, "観戦者の数え: {:?}", rep.counts);
    let _ = writeln!(s, "組み立ての数え: {:?}", rep.asm);
    let _ = writeln!(s, "わざと壊した数: {:?}", rep.faults);
    let _ = writeln!(
        s,
        "送り {} 通（失敗 {}・ガードで止めた {}）",
        rep.sends, rep.send_errors, rep.send_refused
    );
    s
}

/// 記録の CSV から `--compare-spectate` が使う 2 つを取り出したもの。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventsCsv {
    pub steps: Vec<String>,
    /// 最初の行の記録の番号。0 ならリングバッファの先頭から全部受けた。0 でなければ、いまの seg の始まり `Seg(k)` から受けた。行が無ければ None。
    pub first_index: Option<u32>,
}

pub fn parse_events_csv(text: &str) -> Result<EventsCsv, String> {
    let mut lines = text.lines().enumerate();
    match lines.next() {
        Some((_, h)) if h.trim() == EVENTS_CSV_HEADER => {}
        Some((_, h)) => {
            return Err(format!(
                "記録の CSV の見出し {h:?} が {EVENTS_CSV_HEADER:?} でない"
            ));
        }
        None => return Err("記録の CSV が空".to_string()),
    }
    let mut steps = Vec::new();
    let mut first_index = None;
    for (i, l) in lines {
        if l.trim().is_empty() {
            continue;
        }
        let mut f = l.splitn(3, ',');
        let (Some(idx), Some(ty), Some(fields)) = (f.next(), f.next(), f.next()) else {
            return Err(format!("{} 行目の列の数が 3 でない: {l:?}", i + 1));
        };
        if first_index.is_none() {
            first_index = Some(
                idx.trim()
                    .parse::<u32>()
                    .map_err(|_| format!("{} 行目の番号 {idx:?} が読めない", i + 1))?,
            );
        }
        if ty == "step" {
            steps.push(fields.trim().to_string());
        }
    }
    Ok(EventsCsv { steps, first_index })
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::wire::StepKind;

    fn rep() -> WatchReport {
        WatchReport {
            end: WatchEnd::Deadline,
            joined: true,
            host_addr: None,
            frames: vec![
                WFrame {
                    seg: 0,
                    frame: 0,
                    p1: 0x00C4,
                    p2: 0xBEEF,
                },
                WFrame {
                    seg: 1,
                    frame: 0,
                    p1: 1,
                    p2: 2,
                },
            ],
            events: vec![
                (0, WEvent::Menu { p1: 1, p2: 0x10 }),
                (
                    1,
                    WEvent::Step(Step {
                        step: 3,
                        kind: StepKind::Decide,
                        value: 0x0A,
                        screen: 7,
                    }),
                ),
                (2, WEvent::Seg(1)),
                (3, WEvent::End(ByeReason::Finished)),
            ],
            records: 6,
            jumps: 0,
            frames_without_seg: 0,
            counts: WatcherCounts::default(),
            asm: AsmCounts::default(),
            faults: FaultCounts::default(),
            sends: 0,
            send_errors: 0,
            send_refused: 0,
            elapsed_ms: 0,
        }
    }

    #[test]
    fn frames_csv_uses_hex4_columns_like_the_dll() {
        assert_eq!(
            frames_csv(&rep()),
            "seg,frame,p1,p2\n0,0,00C4,BEEF\n1,0,0001,0002\n"
        );
    }

    #[test]
    fn steps_round_trip_through_the_events_csv_and_match_the_log_form() {
        let r = rep();
        let csv = events_csv(&r);
        assert!(
            csv.contains("\n1,step,step=3 kind=Decide value=0x0A screen=7\n"),
            "{csv}"
        );
        assert_eq!(
            parse_events_csv(&csv).unwrap(),
            EventsCsv {
                steps: vec!["step=3 kind=Decide value=0x0A screen=7".to_string()],
                first_index: Some(0)
            }
        );
        let late = "index,type,fields\n7,seg,seg=1\n8,end,reason=Finished\n";
        let e = parse_events_csv(late).unwrap();
        assert_eq!((e.steps.len(), e.first_index), (0, Some(7)));
        assert_eq!(
            parse_events_csv("index,type,fields\n").unwrap().first_index,
            None
        );
        assert!(parse_events_csv("seg,frame,p1,p2\n").is_err());
    }
}
