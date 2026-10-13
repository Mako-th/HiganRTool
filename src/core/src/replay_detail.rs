//! 印（`replay.rs`）に足す、マッチの情報の型と書式。ラウンドごとの記録、同期方式、ビルド番号、開始時刻、通信の要約、接続の中の何マッチ目か。
//!
//! 印のバージョンは 1 のまま、`Mark` の後ろに任意のキーを足す（読み手は知らないキーを読み飛ばす）。値は `replay.rs` の `%XX` の逃がしを通すが、
//! ここの値は数字・`-`・`.` と決まった語だけなので逃がしても変わらない。入れるのは次のキーだけ（`ALLOWED_KEYS`）。
//! IP・ポート・接続コード・ルーム名・接続 ID の欄は持たない。
//!
//! | キー | 値 |
//! |---|---|
//! | `sync` | `rollback` / `delay` |
//! | `build` | `update::BUILD_NUMBER`（10 進。0 は番号を付けていないビルド） |
//! | `start` | マッチの開始時刻（UTC の UNIX 秒、10 進） |
//! | `n` | 接続の中で何マッチ目か（1 から） |
//! | `net` | `<ping 中央値 ms>.<ping 最大 ms>.<止まった回数>.<進め直しの回数>.<進め直しで進めた tick 数>`。測れなかった ping は `-` |
//! | `r1`〜`r16` | ラウンドごと。`<frames>.<勝者>.<ライフ1P>.<ライフ2P>.<C2+C3 1P>.<同 2P>.<ボス 1P>.<同 2P>.<リバーサル 1P>.<同 2P>.<スコア 1P>.<同 2P>`。勝者は `1` / `2` / `-`、読めなかった値は `-` |
//! | `rx` | 上限を越えて記録できなかったラウンドの数（0 のときは書かない） |
//!
//! ゲームのスレッドの外（保管庫のスレッドと検査）で使うので、ヒープを使い、どんなバイト列でもパニックしない。
#![forbid(unsafe_code)]

use crate::replay::MarkError;

/// `rN` のキーの上限。ゲームは Match の勝ちに要る数を 2 に決める（`0x0041AFBA`）ので 3 ラウンドが最大で、上限は余裕。
pub const MAX_ROUND_KEYS: usize = 16;

const ROUND_KEY_NAMES: [&str; MAX_ROUND_KEYS] = [
    "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15",
    "r16",
];

const KEY_SYNC: usize = 0;
const KEY_BUILD: usize = 1;
const KEY_START: usize = 2;
const KEY_N: usize = 3;
const KEY_NET: usize = 4;
const KEY_RX: usize = 5;
const KEY_ROUNDS: usize = 6;
const KEY_COUNT: usize = KEY_ROUNDS + MAX_ROUND_KEYS;

/// このモジュールが書く・読むキー（並びは `values` の添字と同じ）。
pub const ALLOWED_KEYS: [&str; KEY_COUNT] = {
    let mut k = [""; KEY_COUNT];
    k[KEY_SYNC] = "sync";
    k[KEY_BUILD] = "build";
    k[KEY_START] = "start";
    k[KEY_N] = "n";
    k[KEY_NET] = "net";
    k[KEY_RX] = "rx";
    let mut i = 0;
    while i < MAX_ROUND_KEYS {
        k[KEY_ROUNDS + i] = ROUND_KEY_NAMES[i];
        i += 1;
    }
    k
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncKind {
    Rollback,
    Delay,
}

impl SyncKind {
    fn as_str(self) -> &'static str {
        match self {
            SyncKind::Rollback => "rollback",
            SyncKind::Delay => "delay",
        }
    }
}

/// ラウンドの勝者（席の番号。1P / 2P）。勝ち数の増えた席が 1 つに決まらなければ `Unknown`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Winner {
    P1,
    P2,
    Unknown,
}

/// 1 ラウンドの記録。`[0]` が 1P、`[1]` が 2P。`None` は読めなかった値。
/// カウンタ（`c23`・`boss`・`rev`）はそのラウンドの分（境目どうしの差）。ライフとスコアは境目の tick の値そのまま。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundRec {
    /// ラウンドの時間（`round_frames`）。
    pub frames: u32,
    pub winner: Winner,
    pub life: [Option<u32>; 2],
    /// C2 と C3 の数（Player の `spell_attacks`）。
    pub c23: [Option<u32>; 2],
    /// ボスアタックの数（`boss_attacks`）。
    pub boss: [Option<u32>; 2],
    /// リバーサルの数（`boss_reversals`。ボスアタックの内数）。
    pub rev: [Option<u32>; 2],
    /// 画面に出るスコア（内部値 × 10。マッチ通しの値）。
    pub score: [Option<u64>; 2],
}

/// 通信の要約（そのマッチの分だけ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommSummary {
    pub ping_median_ms: Option<u32>,
    pub ping_max_ms: Option<u32>,
    /// 相手の語を待って止まった回数（delay 方式は描画だけにしたフレーム、rollback は入力の地点が待った回数）。
    pub stalls: u32,
    /// 進め直し（書き戻し）の回数と、そのために余分に進めた tick の数。
    pub rewinds: u32,
    pub resim_ticks: u32,
}

/// 印に足す、マッチの情報。全部任意（足りないものは書かない）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchDetail {
    pub sync: Option<SyncKind>,
    pub build: Option<u32>,
    pub start_utc: Option<u64>,
    pub match_no: Option<u32>,
    pub comm: Option<CommSummary>,
    pub rounds: Vec<RoundRec>,
    /// 記録の上限を越えて捨てたラウンドの数。
    pub rounds_dropped: u32,
}

fn opt_u32(v: Option<u32>) -> String {
    v.map_or_else(|| "-".to_string(), |x| x.to_string())
}

fn opt_u64(v: Option<u64>) -> String {
    v.map_or_else(|| "-".to_string(), |x| x.to_string())
}

impl RoundRec {
    pub fn to_value(&self) -> String {
        let w = match self.winner {
            Winner::P1 => "1",
            Winner::P2 => "2",
            Winner::Unknown => "-",
        };
        format!(
            "{}.{}.{}.{}.{}.{}.{}.{}.{}.{}.{}.{}",
            self.frames,
            w,
            opt_u32(self.life[0]),
            opt_u32(self.life[1]),
            opt_u32(self.c23[0]),
            opt_u32(self.c23[1]),
            opt_u32(self.boss[0]),
            opt_u32(self.boss[1]),
            opt_u32(self.rev[0]),
            opt_u32(self.rev[1]),
            opt_u64(self.score[0]),
            opt_u64(self.score[1]),
        )
    }

    /// `to_value` の逆。欄の数が 12 でない・数字でない・範囲を越える値は `None`。
    pub fn parse(v: &[u8]) -> Option<RoundRec> {
        let f: Vec<&[u8]> = v.split(|&b| b == b'.').collect();
        if f.len() != 12 {
            return None;
        }
        let winner = match f[1] {
            b"1" => Winner::P1,
            b"2" => Winner::P2,
            b"-" => Winner::Unknown,
            _ => return None,
        };
        Some(RoundRec {
            frames: digits_u32(f[0])?,
            winner,
            life: [dash_u32(f[2])?, dash_u32(f[3])?],
            c23: [dash_u32(f[4])?, dash_u32(f[5])?],
            boss: [dash_u32(f[6])?, dash_u32(f[7])?],
            rev: [dash_u32(f[8])?, dash_u32(f[9])?],
            score: [dash_u64(f[10])?, dash_u64(f[11])?],
        })
    }
}

impl CommSummary {
    pub fn to_value(&self) -> String {
        format!(
            "{}.{}.{}.{}.{}",
            opt_u32(self.ping_median_ms),
            opt_u32(self.ping_max_ms),
            self.stalls,
            self.rewinds,
            self.resim_ticks
        )
    }

    pub fn parse(v: &[u8]) -> Option<CommSummary> {
        let f: Vec<&[u8]> = v.split(|&b| b == b'.').collect();
        if f.len() != 5 {
            return None;
        }
        Some(CommSummary {
            ping_median_ms: dash_u32(f[0])?,
            ping_max_ms: dash_u32(f[1])?,
            stalls: digits_u32(f[2])?,
            rewinds: digits_u32(f[3])?,
            resim_ticks: digits_u32(f[4])?,
        })
    }
}

/// 数字だけ（`+`・空白・空は不可）の 10 進を読む。
fn digits_u64(b: &[u8]) -> Option<u64> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(b).ok()?.parse().ok()
}

fn digits_u32(b: &[u8]) -> Option<u32> {
    digits_u64(b).and_then(|v| u32::try_from(v).ok())
}

/// `-` は「読めなかった」（外側の `Some(None)`）。数字でも `-` でもなければ外側が `None`。
fn dash_u32(b: &[u8]) -> Option<Option<u32>> {
    if b == b"-" {
        Some(None)
    } else {
        digits_u32(b).map(Some)
    }
}

fn dash_u64(b: &[u8]) -> Option<Option<u64>> {
    if b == b"-" {
        Some(None)
    } else {
        digits_u64(b).map(Some)
    }
}

impl MatchDetail {
    /// 書くキーと値（`ALLOWED_KEYS` の並び）。値は逃がす前。
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(s) = self.sync {
            out.push((ALLOWED_KEYS[KEY_SYNC], s.as_str().to_string()));
        }
        if let Some(b) = self.build {
            out.push((ALLOWED_KEYS[KEY_BUILD], b.to_string()));
        }
        if let Some(t) = self.start_utc {
            out.push((ALLOWED_KEYS[KEY_START], t.to_string()));
        }
        if let Some(n) = self.match_no {
            out.push((ALLOWED_KEYS[KEY_N], n.to_string()));
        }
        if let Some(c) = &self.comm {
            out.push((ALLOWED_KEYS[KEY_NET], c.to_value()));
        }
        if self.rounds_dropped > 0 {
            out.push((ALLOWED_KEYS[KEY_RX], self.rounds_dropped.to_string()));
        }
        for (i, r) in self.rounds.iter().enumerate() {
            if let Some(&key) = ALLOWED_KEYS.get(KEY_ROUNDS + i) {
                out.push((key, r.to_value()));
            }
        }
        out
    }

    /// 書けない値（`rN` のキーが足りない）なら、読むときに壊れていることになるので書く前に断る。
    pub fn check(&self) -> Result<(), MarkError> {
        if self.rounds.len() > MAX_ROUND_KEYS {
            return Err(MarkError::BadValue("rounds"));
        }
        Ok(())
    }
}

/// 読むときに、印の本文から集めたキーの値（逃がしを戻した後）。`MatchDetail` に組み立てる。
pub struct DetailFields {
    values: [Option<Vec<u8>>; KEY_COUNT],
}

impl Default for DetailFields {
    fn default() -> Self {
        DetailFields {
            values: std::array::from_fn(|_| None),
        }
    }
}

/// `key` がこのモジュールのキーなら、その番号（`ALLOWED_KEYS` の添字）。
pub fn key_index(key: &[u8]) -> Option<usize> {
    ALLOWED_KEYS.iter().position(|k| k.as_bytes() == key)
}

pub fn key_name(idx: usize) -> &'static str {
    ALLOWED_KEYS.get(idx).copied().unwrap_or("")
}

impl DetailFields {
    /// 同じキーが 2 回来たら `DuplicateKey`。
    pub fn put(&mut self, idx: usize, value: Vec<u8>) -> Result<(), MarkError> {
        let name = key_name(idx);
        match self.values.get_mut(idx) {
            Some(slot) if slot.is_none() => {
                *slot = Some(value);
                Ok(())
            }
            Some(_) => Err(MarkError::DuplicateKey(name)),
            None => Err(MarkError::BadValue("detail")),
        }
    }

    /// 何も無ければ `Ok(None)`（足す前の印）。値が読めなければその key の `BadValue`。`rN` は `r1` から途切れず並ぶこと。
    pub fn finish(self) -> Result<Option<MatchDetail>, MarkError> {
        if self.values.iter().all(Option::is_none) {
            return Ok(None);
        }
        let get = |i: usize| self.values.get(i).and_then(|v| v.as_deref());
        let bad = |i: usize| MarkError::BadValue(key_name(i));
        let mut d = MatchDetail::default();
        if let Some(v) = get(KEY_SYNC) {
            d.sync = Some(match v {
                b"rollback" => SyncKind::Rollback,
                b"delay" => SyncKind::Delay,
                _ => return Err(bad(KEY_SYNC)),
            });
        }
        if let Some(v) = get(KEY_BUILD) {
            d.build = Some(digits_u32(v).ok_or(bad(KEY_BUILD))?);
        }
        if let Some(v) = get(KEY_START) {
            d.start_utc = Some(digits_u64(v).ok_or(bad(KEY_START))?);
        }
        if let Some(v) = get(KEY_N) {
            d.match_no = Some(digits_u32(v).ok_or(bad(KEY_N))?);
        }
        if let Some(v) = get(KEY_NET) {
            d.comm = Some(CommSummary::parse(v).ok_or(bad(KEY_NET))?);
        }
        if let Some(v) = get(KEY_RX) {
            d.rounds_dropped = digits_u32(v).ok_or(bad(KEY_RX))?;
        }
        let mut gap = false;
        for i in 0..MAX_ROUND_KEYS {
            let idx = KEY_ROUNDS + i;
            match get(idx) {
                Some(v) => {
                    if gap {
                        return Err(bad(idx));
                    }
                    d.rounds.push(RoundRec::parse(v).ok_or(bad(idx))?);
                }
                None => gap = true,
            }
        }
        Ok(Some(d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_round(n: u32) -> RoundRec {
        RoundRec {
            frames: 3600 + n,
            winner: Winner::P2,
            life: [Some(0), Some(7)],
            c23: [Some(2), Some(1)],
            boss: [Some(0), Some(1)],
            rev: [None, Some(0)],
            score: [Some(1_234_560), Some(98_760)],
        }
    }

    #[test]
    fn a_round_value_round_trips_and_has_the_documented_shape() {
        let r = sample_round(0);
        assert_eq!(r.to_value(), "3600.2.0.7.2.1.0.1.-.0.1234560.98760");
        assert_eq!(RoundRec::parse(r.to_value().as_bytes()), Some(r));
        let none = RoundRec {
            winner: Winner::Unknown,
            life: [None; 2],
            c23: [None; 2],
            boss: [None; 2],
            rev: [None; 2],
            score: [None; 2],
            ..r
        };
        assert_eq!(none.to_value(), "3600.-.-.-.-.-.-.-.-.-.-.-");
        assert_eq!(RoundRec::parse(none.to_value().as_bytes()), Some(none));
    }

    #[test]
    fn malformed_round_values_are_refused() {
        let good = sample_round(0).to_value();
        for bad in [
            "",
            "1.2.3",
            &format!("{good}.0"),
            &good.replace("3600", "36x0"),
            &good.replace("3600", "+3600"),
            &good.replace("3600", "4294967296"),
            &good.replace(".2.", ".3."),
            &good.replace("98760", "-1"),
            &good.replace("1234560", ""),
        ] {
            assert_eq!(RoundRec::parse(bad.as_bytes()), None, "{bad:?}");
        }
        assert!(RoundRec::parse(good.as_bytes()).is_some());
    }

    #[test]
    fn comm_values_round_trip_and_refuse_garbage() {
        let c = CommSummary {
            ping_median_ms: Some(31),
            ping_max_ms: Some(120),
            stalls: 4,
            rewinds: 55,
            resim_ticks: 140,
        };
        assert_eq!(c.to_value(), "31.120.4.55.140");
        assert_eq!(CommSummary::parse(b"31.120.4.55.140"), Some(c));
        let unknown = CommSummary {
            ping_median_ms: None,
            ping_max_ms: None,
            ..c
        };
        assert_eq!(unknown.to_value(), "-.-.4.55.140");
        assert_eq!(CommSummary::parse(b"-.-.4.55.140"), Some(unknown));
        for bad in ["", "1.2.3.4", "1.2.3.4.5.6", "1.2.-.4.5", "a.2.3.4.5"] {
            assert_eq!(CommSummary::parse(bad.as_bytes()), None, "{bad:?}");
        }
    }

    #[test]
    fn fields_follow_the_allowed_key_list_and_nothing_else() {
        let d = MatchDetail {
            sync: Some(SyncKind::Rollback),
            build: Some(403_920),
            start_utc: Some(1_790_000_000),
            match_no: Some(2),
            comm: Some(CommSummary {
                ping_median_ms: Some(30),
                ping_max_ms: Some(90),
                stalls: 0,
                rewinds: 3,
                resim_ticks: 9,
            }),
            rounds: vec![sample_round(0), sample_round(1), sample_round(2)],
            rounds_dropped: 1,
        };
        let keys: Vec<&str> = d.fields().iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            ["sync", "build", "start", "n", "net", "rx", "r1", "r2", "r3"]
        );
        for k in &keys {
            assert!(ALLOWED_KEYS.contains(k), "{k}");
        }
        // 接続 ID・ルーム名・接続コード・IP・ポートの欄は無い。
        for k in ALLOWED_KEYS {
            for forbidden in ["room", "conn", "code", "ip", "addr", "port", "host", "sid"] {
                assert_ne!(k, forbidden);
            }
        }
        assert_eq!(MatchDetail::default().fields(), vec![]);
    }

    fn feed(pairs: &[(&str, &str)]) -> DetailFields {
        let mut f = DetailFields::default();
        for (k, v) in pairs {
            f.put(key_index(k.as_bytes()).unwrap(), v.as_bytes().to_vec())
                .unwrap();
        }
        f
    }

    #[test]
    fn finish_builds_the_detail_and_refuses_broken_values() {
        assert_eq!(DetailFields::default().finish(), Ok(None));
        let r1 = sample_round(0).to_value();
        let r2 = sample_round(1).to_value();
        let d = feed(&[
            ("r2", &r2),
            ("r1", &r1),
            ("sync", "delay"),
            ("build", "7"),
            ("start", "1790000000"),
            ("n", "3"),
        ])
        .finish()
        .unwrap()
        .unwrap();
        assert_eq!(d.rounds, vec![sample_round(0), sample_round(1)]);
        assert_eq!(d.sync, Some(SyncKind::Delay));
        assert_eq!((d.build, d.match_no), (Some(7), Some(3)));
        assert_eq!(d.comm, None);

        let err = |pairs: &[(&str, &str)]| feed(pairs).finish().unwrap_err();
        assert_eq!(err(&[("sync", "Rollback")]), MarkError::BadValue("sync"));
        assert_eq!(err(&[("build", "-1")]), MarkError::BadValue("build"));
        assert_eq!(err(&[("start", "1x")]), MarkError::BadValue("start"));
        assert_eq!(err(&[("n", "")]), MarkError::BadValue("n"));
        assert_eq!(err(&[("net", "1.2")]), MarkError::BadValue("net"));
        assert_eq!(err(&[("rx", "z")]), MarkError::BadValue("rx"));
        assert_eq!(err(&[("r1", "1.2")]), MarkError::BadValue("r1"));
        // r1 が無いのに r2 がある（途切れている）。
        assert_eq!(err(&[("r2", &r2)]), MarkError::BadValue("r2"));
        assert_eq!(err(&[("r1", &r1), ("r3", &r2)]), MarkError::BadValue("r3"));
    }

    #[test]
    fn a_key_given_twice_is_a_duplicate() {
        let mut f = feed(&[("n", "1")]);
        assert_eq!(
            f.put(key_index(b"n").unwrap(), b"2".to_vec()),
            Err(MarkError::DuplicateKey("n"))
        );
    }

    #[test]
    fn too_many_rounds_cannot_be_written() {
        let mut d = MatchDetail {
            rounds: vec![sample_round(0); MAX_ROUND_KEYS],
            ..MatchDetail::default()
        };
        assert_eq!(d.check(), Ok(()));
        assert_eq!(d.fields().len(), MAX_ROUND_KEYS);
        d.rounds.push(sample_round(0));
        assert_eq!(d.check(), Err(MarkError::BadValue("rounds")));
    }

    #[test]
    fn unknown_keys_are_not_taken() {
        assert_eq!(key_index(b"peer"), None);
        assert_eq!(key_index(b"r17"), None);
        assert_eq!(key_index(b"r16"), Some(KEY_ROUNDS + 15));
        assert_eq!(key_index(b""), None);
    }
}
