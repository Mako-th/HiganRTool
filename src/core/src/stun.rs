//! STUN の Binding だけ（RFC 5389 / RFC 8489）: Binding Request の組み立てと Binding Success Response の解析。
//! 候補アドレスの選び方・STUN サーバー・ソケット・乱数は net の層の仕事（`link.rs` と同じ「セッションは持たない」流儀）。
//!
//! - 属性は付けない（MESSAGE-INTEGRITY / FINGERPRINT も送らない）。transaction id は呼び手が渡す（core は乱数を持たない）。
//! - 解析は XOR-MAPPED-ADDRESS（0x0020）を優先し、無ければ MAPPED-ADDRESS（0x0001）。知らない属性は 4 バイト境界の
//!   パディングごと読み飛ばす。MESSAGE-INTEGRITY / FINGERPRINT は検証しない。
//! - 問い合わせの送り直しと答え合わせ（`Query`）、検査の偽の応答役が使う応答の組み立て（`encode_binding_success`）、
//!   2 か所の STUN の答えの比べ（`compare_mappings`・`Mapping`・`SecondProbe`）も持つ。
//!   受信ソケットで HGRT と STUN を分けるのは `wire::classify`。
//!
//! 接続の手順でだけ呼ぶ想定だが、unsafe 禁止・パニックしうる添字と unwrap は置かない。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// transaction id の長さ（96 bit。RFC 5389 §6）。
pub const TRANSACTION_ID_LEN: usize = 12;
pub type TransactionId = [u8; TRANSACTION_ID_LEN];

/// magic cookie（RFC 5389 §6）。
pub const MAGIC_COOKIE: u32 = 0x2112_A442;
pub const HEADER_LEN: usize = 20;

// message type（RFC 5389 §6 / §18.1）。
const TYPE_BINDING_REQUEST: u16 = 0x0001;
const TYPE_BINDING_SUCCESS_RESPONSE: u16 = 0x0101;
const TYPE_BINDING_ERROR_RESPONSE: u16 = 0x0111;

// 属性の type（RFC 5389 §15.1・§15.2）。
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

// アドレスの family（RFC 5389 §15.1）。
const FAMILY_IPV4: u8 = 0x01;
const FAMILY_IPV6: u8 = 0x02;

/// Binding Request（属性なし。20 B ちょうど）を作る。
pub fn encode_binding_request(txn: TransactionId) -> [u8; HEADER_LEN] {
    let mut buf = [0u8; HEADER_LEN];
    buf[0..2].copy_from_slice(&TYPE_BINDING_REQUEST.to_be_bytes());
    buf[2..4].copy_from_slice(&0u16.to_be_bytes());
    buf[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    buf[8..20].copy_from_slice(&txn);
    buf
}

/// `decode_binding_response` が返すエラー。パニックの代わりに区別できる形で返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StunError {
    Truncated,
    /// message type の先頭 2 bit が 0 でない（STUN 以外との識別。RFC 5389 §6）。
    BadLeadingBits,
    BadCookie,
    LengthNotMultipleOf4,
    LengthMismatch,
    AttrOverrun,
    UnknownFamily,
    /// アドレス属性の value の長さが、family（IPv4 なら 8 B、IPv6 なら 20 B）に合わない。
    MalformedAddress,
    /// Binding Error Response（`0x0111`）だった。
    ErrorResponse,
    UnexpectedType(u16),
    TransactionMismatch,
    NoAddress,
}

struct Header {
    msg_type: u16,
    msg_len: usize,
    cookie: u32,
    txn: TransactionId,
}

/// ヘッダ（20 B）だけを読み、構造的な検査（先頭 2 bit・cookie・4 の倍数・本体の長さ）を通す。type の意味と
/// transaction id の一致は呼び手が見る。
fn decode_header(bytes: &[u8]) -> Result<Header, StunError> {
    if bytes.len() < HEADER_LEN {
        return Err(StunError::Truncated);
    }
    let msg_type = u16::from_be_bytes([bytes[0], bytes[1]]);
    // message-type は 14 bit の意味しか持たず、先頭 2 bit は常に 0（STUN の目印）。
    if msg_type & 0xC000 != 0 {
        return Err(StunError::BadLeadingBits);
    }
    let msg_len = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
    let cookie = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if cookie != MAGIC_COOKIE {
        return Err(StunError::BadCookie);
    }
    if !msg_len.is_multiple_of(4) {
        return Err(StunError::LengthNotMultipleOf4);
    }
    // 本体の長さは、渡されたバイト列のうちヘッダの後ろの分とちょうど合っていなければならない（余りが付いた別のプロトコルや途中で切れたものを弾く）。
    if bytes.len() != HEADER_LEN + msg_len {
        return Err(StunError::LengthMismatch);
    }
    let mut txn = [0u8; TRANSACTION_ID_LEN];
    txn.copy_from_slice(&bytes[8..HEADER_LEN]);
    Ok(Header {
        msg_type,
        msg_len,
        cookie,
        txn,
    })
}

/// MAPPED-ADDRESS / XOR-MAPPED-ADDRESS の value のうち、family（1 B）より後ろだけを渡す。`xor_key` が `Some` なら
/// XOR-MAPPED-ADDRESS として解く（port は magic cookie の上位 16 bit、IPv6 の address は cookie ＋ transaction id と
/// XOR する。RFC 5389 §15.2）。`None` なら MAPPED-ADDRESS としてそのまま読む（§15.1）。
fn decode_addr_body(
    family: u8,
    rest: &[u8],
    xor_key: Option<&[u8; 16]>,
) -> Result<SocketAddr, StunError> {
    match family {
        FAMILY_IPV4 => {
            if rest.len() != 6 {
                return Err(StunError::MalformedAddress);
            }
            let raw_port = u16::from_be_bytes([rest[0], rest[1]]);
            let mut octets = [rest[2], rest[3], rest[4], rest[5]];
            let port = match xor_key {
                Some(key) => {
                    for (o, k) in octets.iter_mut().zip(key[0..4].iter()) {
                        *o ^= k;
                    }
                    raw_port ^ u16::from_be_bytes([key[0], key[1]])
                }
                None => raw_port,
            };
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port))
        }
        FAMILY_IPV6 => {
            if rest.len() != 18 {
                return Err(StunError::MalformedAddress);
            }
            let raw_port = u16::from_be_bytes([rest[0], rest[1]]);
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&rest[2..18]);
            let port = match xor_key {
                Some(key) => {
                    for (o, k) in octets.iter_mut().zip(key.iter()) {
                        *o ^= k;
                    }
                    raw_port ^ u16::from_be_bytes([key[0], key[1]])
                }
                None => raw_port,
            };
            Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
        }
        _ => Err(StunError::UnknownFamily),
    }
}

fn decode_mapped_address(value: &[u8]) -> Result<SocketAddr, StunError> {
    if value.len() < 2 {
        return Err(StunError::MalformedAddress);
    }
    let family = value[1];
    decode_addr_body(family, &value[2..], None)
}

/// XOR-MAPPED-ADDRESS の属性 value を解く（`cookie` と `txn` はヘッダの値）。
fn decode_xor_mapped_address(
    value: &[u8],
    cookie: u32,
    txn: &TransactionId,
) -> Result<SocketAddr, StunError> {
    if value.len() < 2 {
        return Err(StunError::MalformedAddress);
    }
    let family = value[1];
    let mut key = [0u8; 16];
    key[0..4].copy_from_slice(&cookie.to_be_bytes());
    key[4..16].copy_from_slice(txn);
    decode_addr_body(family, &value[2..], Some(&key))
}

/// Binding Success Response を解析し、STUN サーバーから見えた自分のアドレスを返す。`expected_txn` は要求に使った値。
pub fn decode_binding_response(
    bytes: &[u8],
    expected_txn: &TransactionId,
) -> Result<SocketAddr, StunError> {
    let hdr = decode_header(bytes)?;
    if hdr.txn != *expected_txn {
        return Err(StunError::TransactionMismatch);
    }
    match hdr.msg_type {
        TYPE_BINDING_SUCCESS_RESPONSE => {}
        TYPE_BINDING_ERROR_RESPONSE => return Err(StunError::ErrorResponse),
        other => return Err(StunError::UnexpectedType(other)),
    }
    let body = &bytes[HEADER_LEN..HEADER_LEN + hdr.msg_len];

    // 属性を歩く。知らない属性は読み飛ばす。最初の XOR-MAPPED-ADDRESS / MAPPED-ADDRESS の value だけ取っておき、読み終えてから
    // XOR を優先して解く。未知の属性の中身を読まずに次へ跳べるよう、枠（type・length・パディング）はどの属性も同じ検査をする。
    let mut xor_val: Option<&[u8]> = None;
    let mut mapped_val: Option<&[u8]> = None;
    let mut i = 0usize;
    while i + 4 <= body.len() {
        let atype = u16::from_be_bytes([body[i], body[i + 1]]);
        let alen = u16::from_be_bytes([body[i + 2], body[i + 3]]) as usize;
        let start = i + 4;
        let end = match start.checked_add(alen) {
            Some(e) => e,
            None => return Err(StunError::AttrOverrun),
        };
        if end > body.len() {
            return Err(StunError::AttrOverrun);
        }
        let val = &body[start..end];
        match atype {
            ATTR_XOR_MAPPED_ADDRESS if xor_val.is_none() => xor_val = Some(val),
            ATTR_MAPPED_ADDRESS if mapped_val.is_none() => mapped_val = Some(val),
            _ => {}
        }
        // 次の属性は 4 バイト境界から（パディングは length に数えない）。
        let padded = alen.div_ceil(4) * 4;
        // 加算が跳ねてもループの入口の `i + 4 <= body.len()` が守る。
        i = start.saturating_add(padded);
    }

    if let Some(v) = xor_val {
        return decode_xor_mapped_address(v, hdr.cookie, &hdr.txn);
    }
    if let Some(v) = mapped_val {
        return decode_mapped_address(v);
    }
    Err(StunError::NoAddress)
}

// ---------------- 偽の応答役と問い合わせ ----------------

/// `encode_binding_success` が書く最長（IPv6 の XOR-MAPPED-ADDRESS 1 つ）。
pub const MAX_SUCCESS_LEN: usize = HEADER_LEN + 4 + 20;

/// Binding Request なら transaction id を返す（検査の偽の応答役が使う。属性は見ない）。
pub fn binding_request_txn(bytes: &[u8]) -> Option<TransactionId> {
    let hdr = decode_header(bytes).ok()?;
    (hdr.msg_type == TYPE_BINDING_REQUEST).then_some(hdr.txn)
}

/// Binding Success Response（XOR-MAPPED-ADDRESS 1 つだけ。RFC 5389 §15.2）を作る。検査の偽の応答役が使う。戻り値は書いた長さ。
pub fn encode_binding_success(
    txn: TransactionId,
    mapped: SocketAddr,
    out: &mut [u8; MAX_SUCCESS_LEN],
) -> usize {
    let mut key = [0u8; 16];
    key[0..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    key[4..16].copy_from_slice(&txn);
    let xport = mapped.port() ^ u16::from_be_bytes([key[0], key[1]]);
    let (family, addr_len) = match mapped.ip() {
        IpAddr::V4(_) => (FAMILY_IPV4, 4usize),
        IpAddr::V6(_) => (FAMILY_IPV6, 16usize),
    };
    let value_len = 4 + addr_len;
    out[0..2].copy_from_slice(&TYPE_BINDING_SUCCESS_RESPONSE.to_be_bytes());
    out[2..4].copy_from_slice(&((4 + value_len) as u16).to_be_bytes());
    out[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out[8..20].copy_from_slice(&txn);
    out[20..22].copy_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
    out[22..24].copy_from_slice(&(value_len as u16).to_be_bytes());
    out[24] = 0;
    out[25] = family;
    out[26..28].copy_from_slice(&xport.to_be_bytes());
    match mapped.ip() {
        IpAddr::V4(ip) => {
            for (i, (o, k)) in ip.octets().iter().zip(key.iter()).enumerate() {
                out[28 + i] = o ^ k;
            }
        }
        IpAddr::V6(ip) => {
            for (i, (o, k)) in ip.octets().iter().zip(key.iter()).enumerate() {
                out[28 + i] = o ^ k;
            }
        }
    }
    HEADER_LEN + 4 + value_len
}

/// 問い合わせの送り直しの間隔（ms）。RFC 5389 §7.2.1 の既定（RTO 500 ms から倍々で 7 回。計 39.5 秒）は接続の手順の
/// 時間切れ（10 秒。`handshake::TIMEOUT_MS`）より長いので、250 ms ごとに 8 回（2 秒）で打ち切る。
pub const QUERY_INTERVAL_MS: u32 = 250;
pub const QUERY_TRIES: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryState {
    Waiting,
    Done(SocketAddr),
    TimedOut,
}

/// 1 つの STUN サーバーへの Binding の問い合わせ（ソケットも時計も乱数も持たない）。呼び手は `poll_send` が返したものを
/// 送り、受けた STUN を `on_datagram` へ渡す。
#[derive(Debug, Clone, Copy)]
pub struct Query {
    txn: TransactionId,
    sent: u32,
    last: Option<u32>,
    state: QueryState,
    /// 違う transaction id・壊れた応答（数えて捨てる）。
    pub ignored: u32,
}

impl Query {
    pub fn new(txn: TransactionId) -> Query {
        Query {
            txn,
            sent: 0,
            last: None,
            state: QueryState::Waiting,
            ignored: 0,
        }
    }

    pub fn state(&self) -> QueryState {
        self.state
    }

    /// 送るものがあれば返す（最初はすぐ、以後 `QUERY_INTERVAL_MS` ごと）。`QUERY_TRIES` 回送った後、もう 1 間隔待っても
    /// 答えが無ければ `TimedOut`。
    pub fn poll_send(&mut self, now: u32) -> Option<[u8; HEADER_LEN]> {
        if self.state != QueryState::Waiting {
            return None;
        }
        if let Some(t) = self.last
            && now.wrapping_sub(t) < QUERY_INTERVAL_MS
        {
            return None;
        }
        if self.sent >= QUERY_TRIES {
            self.state = QueryState::TimedOut;
            return None;
        }
        self.sent += 1;
        self.last = Some(now);
        Some(encode_binding_request(self.txn))
    }

    /// 受けた 1 データグラム（STUN と分けたもの）。答えなら `Done` にして true。
    pub fn on_datagram(&mut self, bytes: &[u8]) -> bool {
        if self.state != QueryState::Waiting {
            return false;
        }
        match decode_binding_response(bytes, &self.txn) {
            Ok(a) => {
                self.state = QueryState::Done(a);
                true
            }
            Err(_) => {
                self.ignored = self.ignored.saturating_add(1);
                false
            }
        }
    }
}

/// 待っている間の聞き直しの間隔（ms）。家庭用ルーターの UDP の対応づけの寿命は機種で違い、短いものは 30 秒前後と言われる
/// （RFC 4787 §4.3 は 2 分以上を求めるが守らない機種がある）ので、その半分にした（ルーターごとの実際の寿命は測っていない）。
/// マッチ中は聞かない（繋がったら止める）。外へ出るのは待っている間だけで、同じ STUN サーバーへ 20 B が 15 秒に 1 回。
pub const KEEP_INTERVAL_MS: u32 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remap {
    Same,
    /// アドレスは同じでポートだけ変わった（ルーターが対応づけを忘れて付け直した形）。
    PortOnly,
    /// アドレスが変わった（ポートも変わったかは問わない。回線の付け替え・CGNAT の別の出口など）。
    Address,
}

pub fn remap(before: SocketAddr, after: SocketAddr) -> Remap {
    if before.ip() != after.ip() {
        Remap::Address
    } else if before.port() != after.port() {
        Remap::PortOnly
    } else {
        Remap::Same
    }
}

/// 聞き直しの数。アドレス・ポートの値は持たない（本ログに人を特定する値を書かないため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeepCounts {
    pub sent: u32,
    pub answered: u32,
    /// 答えにならなかった STUN（違う transaction id・壊れたもの・答えの二重）。
    pub ignored: u32,
    /// 前に見えていた外側のアドレスから変わった回数（ポートだけ / アドレスも）。一度変わってそのままなら 1 回。
    pub changed_port: u32,
    pub changed_addr: u32,
    /// 直近の答えが、自分のコードに載せたアドレスと違うか（まだ答えが無ければ false）。
    pub differs_from_code: bool,
}

/// 待っている間、同じ STUN サーバーへ一定の間隔で Binding を聞き直す状態機械（`Query` と同じ流儀）。目的はルーターの
/// 対応づけを忘れさせないことと、外側のアドレスが変わったかを測ること。呼び手は `due` が true のとき `send` の戻りを送り、
/// 受けた STUN を `on_datagram` へ渡す。最初の問い合わせは作った時刻から 1 間隔後。問い合わせは 1 つだけ覚える
/// （次を送ると前の答えは捨てる）。
#[derive(Debug, Clone, Copy)]
pub struct Keep {
    interval_ms: u32,
    /// 自分のコードに載せた外側のアドレス（最初の `Query` の答え）。
    code: SocketAddr,
    /// 直近に見えた外側のアドレス（前の答えと比べて変わったかを数える）。
    seen: SocketAddr,
    txn: Option<TransactionId>,
    last_send: u32,
    counts: KeepCounts,
}

impl Keep {
    pub fn new(mapped: SocketAddr, now: u32, interval_ms: u32) -> Keep {
        Keep {
            interval_ms,
            code: mapped,
            seen: mapped,
            txn: None,
            last_send: now,
            counts: KeepCounts::default(),
        }
    }

    pub fn due(&self, now: u32) -> bool {
        now.wrapping_sub(self.last_send) >= self.interval_ms
    }

    /// 問い合わせを 1 通作る（`due` が true のときに呼ぶ）。
    pub fn send(&mut self, now: u32, txn: TransactionId) -> [u8; HEADER_LEN] {
        self.txn = Some(txn);
        self.last_send = now;
        self.counts.sent = self.counts.sent.saturating_add(1);
        encode_binding_request(txn)
    }

    /// 受けた STUN。答えとして使えたら、前の答えからの変わり方を返す。
    pub fn on_datagram(&mut self, bytes: &[u8]) -> Option<Remap> {
        let answer = self
            .txn
            .and_then(|txn| decode_binding_response(bytes, &txn).ok());
        let Some(a) = answer else {
            self.counts.ignored = self.counts.ignored.saturating_add(1);
            return None;
        };
        // 答えは 1 回だけ使う。
        self.txn = None;
        self.counts.answered = self.counts.answered.saturating_add(1);
        let r = remap(self.seen, a);
        match r {
            Remap::Same => {}
            Remap::PortOnly => {
                self.counts.changed_port = self.counts.changed_port.saturating_add(1);
            }
            Remap::Address => {
                self.counts.changed_addr = self.counts.changed_addr.saturating_add(1);
            }
        }
        self.seen = a;
        self.counts.differs_from_code = a != self.code;
        Some(r)
    }

    pub fn counts(&self) -> KeepCounts {
        self.counts
    }
}

// ---------------- 2 か所の STUN の答えの比べ ----------------

/// 同じ待ち受けソケットから 2 か所の STUN へ聞いて、見えた外側のアドレスを比べた結果。ルーターが「宛先ごとに外側のポートを変える型」
/// （RFC 4787 の endpoint-dependent mapping。俗に対称型）かを見分ける。違えば、STUN で見えたアドレスは STUN サーバー向けの穴で、
/// 相手には使えない。`peer_sim` の `--stun-probe` も同じ比べを使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapping {
    Same,
    /// アドレスは同じでポートだけ違う（宛先ごとに外側のポートを変える型）。
    PortOnly,
    /// ポートは同じでアドレスだけ違う（宛先によって出口のアドレスが変わる回線）。
    AddrOnly,
    Both,
}

impl Mapping {
    /// 宛先によって見えるアドレスが変わるか（STUN で見えたアドレスを相手に使えない見込みか）。
    pub fn depends_on_destination(self) -> bool {
        self != Mapping::Same
    }

    pub fn port_differs(self) -> bool {
        matches!(self, Mapping::PortOnly | Mapping::Both)
    }
}

/// 2 つの「見えたアドレス」を比べる。族（IPv4 / IPv6）が違えば None。引数の順に依らない。
pub fn compare_mappings(a: SocketAddr, b: SocketAddr) -> Option<Mapping> {
    if a.is_ipv4() != b.is_ipv4() {
        return None;
    }
    Some(match (a.ip() != b.ip(), a.port() != b.port()) {
        (false, false) => Mapping::Same,
        (false, true) => Mapping::PortOnly,
        (true, false) => Mapping::AddrOnly,
        (true, true) => Mapping::Both,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecondOutcome {
    Compared(Mapping),
    /// 答えは来たが、族が 1 か所目と違い比べられなかった。
    Incomparable,
    NoAnswer,
}

/// 2 か所目の STUN への問い合わせ 1 回分（`Query` ＋ 1 か所目の答えとの比べ）。呼び手は `poll_send` が返したものを 2 か所目の
/// サーバーへ送り、そのサーバーから来た STUN を `on_datagram` へ渡し、`outcome` が Some になったら止める。
/// 偽の答え・壊れた答えは `Query` が捨てて数える。送り元の確認は呼び手の仕事。
#[derive(Debug, Clone, Copy)]
pub struct SecondProbe {
    query: Query,
    first: SocketAddr,
    done: Option<SecondOutcome>,
}

impl SecondProbe {
    pub fn new(txn: TransactionId, first: SocketAddr) -> SecondProbe {
        SecondProbe {
            query: Query::new(txn),
            first,
            done: None,
        }
    }

    /// 送るものがあれば返す（間隔と回数は `Query` と同じ）。
    pub fn poll_send(&mut self, now: u32) -> Option<[u8; HEADER_LEN]> {
        if self.done.is_some() {
            return None;
        }
        let req = self.query.poll_send(now);
        if self.query.state() == QueryState::TimedOut {
            self.done = Some(SecondOutcome::NoAnswer);
        }
        req
    }

    pub fn on_datagram(&mut self, bytes: &[u8]) -> bool {
        if self.done.is_some() || !self.query.on_datagram(bytes) {
            return false;
        }
        if let QueryState::Done(second) = self.query.state() {
            self.done = Some(match compare_mappings(self.first, second) {
                Some(m) => SecondOutcome::Compared(m),
                None => SecondOutcome::Incomparable,
            });
        }
        true
    }

    pub fn outcome(&self) -> Option<SecondOutcome> {
        self.done
    }

    pub fn ignored(&self) -> u32 {
        self.query.ignored
    }
}

#[cfg(test)]
mod tests {
    //! 期待値は自分で組み立てた golden（手計算した XOR の鍵で答え合わせ）と RFC 5769 §2.2・§2.3 の標本。
    //! 実装の往復だけで正しさを主張しない。毒標本（1 か所だけ壊す）→ 往復とでたらめの大量件数。
    use super::*;

    // ---------- golden 1: IPv4（自分で組み立てた。RFC 5769 の値ではない） ----------
    // アドレス 203.0.113.5:4242 を XOR-MAPPED-ADDRESS で載せる。
    // X-Port  = 0x1092 (=4242) ^ 0x2112 (cookie の上位16 bit) = 0x3180
    // X-Addr  = 0xCB007105 (=203.0.113.5) ^ 0x2112A442 = 0xEA12D547

    const TXN1: TransactionId = [
        0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
    ];

    #[rustfmt::skip]
    const IPV4_XOR_RESPONSE: [u8; 32] = [
        // ヘッダ（20 B）
        0x01, 0x01, // type = Binding Success Response
        0x00, 0x0C, // length = 12（属性 1 つ、value 8B + 属性ヘッダ 4B）
        0x21, 0x12, 0xA4, 0x42, // magic cookie
        0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, // transaction id
        // XOR-MAPPED-ADDRESS
        0x00, 0x20, // type
        0x00, 0x08, // length = 8
        0x00, 0x01, // reserved, family = IPv4
        0x31, 0x80, // X-Port
        0xEA, 0x12, 0xD5, 0x47, // X-Address
    ];

    #[test]
    fn golden_ipv4_xor_mapped_address() {
        let addr = decode_binding_response(&IPV4_XOR_RESPONSE, &TXN1).expect("golden が解けない");
        assert_eq!(addr, SocketAddr::from(([203, 0, 113, 5], 4242)));
    }

    // ---------- golden 2: IPv6（自分で組み立てた。RFC 5769 の値ではない） ----------
    // アドレス [2001:db8:85a3::8a2e:370:7334]:5000 を XOR-MAPPED-ADDRESS で載せる。
    // 鍵は cookie（4B）+ transaction id（12B）の 16B。X-Port と X-Address は本文中の doc の通り

    const TXN2: TransactionId = [
        0x9F, 0x8E, 0x7D, 0x6C, 0x5B, 0x4A, 0x39, 0x28, 0x17, 0x06, 0xF5, 0xE4,
    ];

    #[rustfmt::skip]
    const IPV6_XOR_RESPONSE: [u8; 44] = [
        // ヘッダ（20 B）
        0x01, 0x01, // type = Binding Success Response
        0x00, 0x18, // length = 24（属性ヘッダ 4B + value 20B）
        0x21, 0x12, 0xA4, 0x42, // magic cookie
        0x9F, 0x8E, 0x7D, 0x6C, 0x5B, 0x4A, 0x39, 0x28, 0x17, 0x06, 0xF5, 0xE4, // transaction id
        // XOR-MAPPED-ADDRESS
        0x00, 0x20, // type
        0x00, 0x14, // length = 20
        0x00, 0x02, // reserved, family = IPv6
        0x32, 0x9A, // X-Port
        0x01, 0x13, 0xA9, 0xFA, 0x1A, 0x2D, 0x7D, 0x6C,
        0x5B, 0x4A, 0xB3, 0x06, 0x14, 0x76, 0x86, 0xD0, // X-Address
    ];

    #[test]
    fn golden_ipv6_xor_mapped_address() {
        let addr = decode_binding_response(&IPV6_XOR_RESPONSE, &TXN2).expect("golden が解けない");
        let want_ip: Ipv6Addr = "2001:db8:85a3::8a2e:370:7334".parse().unwrap();
        assert_eq!(addr, SocketAddr::new(IpAddr::V6(want_ip), 5000));
    }

    // ---------- golden 4・5: RFC 5769 §2.2・§2.3 の標本（本文の 16 進のダンプから転記した） ----------

    const RFC5769_TXN: TransactionId = [
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
    ];

    #[rustfmt::skip]
    const RFC5769_IPV4_RESPONSE: [u8; 80] = [
        0x01, 0x01, 0x00, 0x3c, // Response type and message length
        0x21, 0x12, 0xa4, 0x42, // Magic cookie
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae, // Transaction ID
        0x80, 0x22, 0x00, 0x0b, // SOFTWARE attribute header
        0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63, 0x74, 0x6f, 0x72, 0x20, // UTF-8 server name
        0x00, 0x20, 0x00, 0x08, // XOR-MAPPED-ADDRESS attribute header
        0x00, 0x01, 0xa1, 0x47, // Address family (IPv4) and xor'd mapped port number
        0xe1, 0x12, 0xa6, 0x43, // Xor'd mapped IPv4 address
        0x00, 0x08, 0x00, 0x14, // MESSAGE-INTEGRITY attribute header
        0x2b, 0x91, 0xf5, 0x99, 0xfd, 0x9e, 0x90, 0xc3, 0x8c, 0x74,
        0x89, 0xf9, 0x2a, 0xf9, 0xba, 0x53, 0xf0, 0x6b, 0xe7, 0xd7, // HMAC-SHA1 fingerprint
        0x80, 0x28, 0x00, 0x04, // FINGERPRINT attribute header
        0xc0, 0x7d, 0x4c, 0x96, // CRC32 fingerprint
    ];

    #[rustfmt::skip]
    const RFC5769_IPV6_RESPONSE: [u8; 92] = [
        0x01, 0x01, 0x00, 0x48, // Response type and message length
        0x21, 0x12, 0xa4, 0x42, // Magic cookie
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae, // Transaction ID
        0x80, 0x22, 0x00, 0x0b, // SOFTWARE attribute header
        0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63, 0x74, 0x6f, 0x72, 0x20, // UTF-8 server name
        0x00, 0x20, 0x00, 0x14, // XOR-MAPPED-ADDRESS attribute header
        0x00, 0x02, 0xa1, 0x47, // Address family (IPv6) and xor'd mapped port number
        0x01, 0x13, 0xa9, 0xfa, 0xa5, 0xd3, 0xf1, 0x79,
        0xbc, 0x25, 0xf4, 0xb5, 0xbe, 0xd2, 0xb9, 0xd9, // Xor'd mapped IPv6 address
        0x00, 0x08, 0x00, 0x14, // MESSAGE-INTEGRITY attribute header
        0xa3, 0x82, 0x95, 0x4e, 0x4b, 0xe6, 0x7b, 0xf1, 0x17, 0x84,
        0xc9, 0x7c, 0x82, 0x92, 0xc2, 0x75, 0xbf, 0xe3, 0xed, 0x41, // HMAC-SHA1 fingerprint
        0x80, 0x28, 0x00, 0x04, // FINGERPRINT attribute header
        0xc8, 0xfb, 0x0b, 0x4c, // CRC32 fingerprint
    ];

    #[test]
    fn golden_rfc5769_ipv4_response() {
        let addr = decode_binding_response(&RFC5769_IPV4_RESPONSE, &RFC5769_TXN)
            .expect("RFC 5769 §2.2 が解けない");
        assert_eq!(addr, SocketAddr::from(([192, 0, 2, 1], 32853)));
    }

    #[test]
    fn golden_rfc5769_ipv6_response() {
        let addr = decode_binding_response(&RFC5769_IPV6_RESPONSE, &RFC5769_TXN)
            .expect("RFC 5769 §2.3 が解けない");
        let want_ip: Ipv6Addr = "2001:db8:1234:5678:11:2233:4455:6677".parse().unwrap();
        assert_eq!(addr, SocketAddr::new(IpAddr::V6(want_ip), 32853));
    }

    /// XOR-MAPPED-ADDRESS が無いときの MAPPED-ADDRESS へのフォールバック（IPv4 golden の後ろに MAPPED-ADDRESS だけを付ける）。
    #[rustfmt::skip]
    const IPV4_MAPPED_ONLY_RESPONSE: [u8; 32] = [
        0x01, 0x01,
        0x00, 0x0C,
        0x21, 0x12, 0xA4, 0x42,
        0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
        // MAPPED-ADDRESS（XOR しない生の値）
        0x00, 0x01, // type
        0x00, 0x08, // length = 8
        0x00, 0x01, // reserved, family = IPv4
        0x10, 0x92, // port = 4242（生の値。XOR しない）
        0xCB, 0x00, 0x71, 0x05, // address = 203.0.113.5
    ];

    #[test]
    fn golden_ipv4_mapped_address_fallback() {
        let addr =
            decode_binding_response(&IPV4_MAPPED_ONLY_RESPONSE, &TXN1).expect("golden が解けない");
        assert_eq!(addr, SocketAddr::from(([203, 0, 113, 5], 4242)));
    }

    /// 両方の属性がある応答では XOR-MAPPED-ADDRESS を優先する（MAPPED-ADDRESS はわざと違うアドレス）。
    #[rustfmt::skip]
    const BOTH_ATTRS_RESPONSE: [u8; 44] = [
        0x01, 0x01,
        0x00, 0x18, // length = 24（MAPPED 12B + XOR 12B）
        0x21, 0x12, 0xA4, 0x42,
        0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
        // MAPPED-ADDRESS（10.0.0.9:1。わざと XOR-MAPPED-ADDRESS と違う値）
        0x00, 0x01,
        0x00, 0x08,
        0x00, 0x01,
        0x00, 0x01, // port = 1
        0x0A, 0x00, 0x00, 0x09, // address = 10.0.0.9
        // XOR-MAPPED-ADDRESS（golden 1 と同じ 203.0.113.5:4242）
        0x00, 0x20,
        0x00, 0x08,
        0x00, 0x01,
        0x31, 0x80,
        0xEA, 0x12, 0xD5, 0x47,
    ];

    #[test]
    fn xor_mapped_address_is_preferred_over_mapped_address() {
        let addr = decode_binding_response(&BOTH_ATTRS_RESPONSE, &TXN1).expect("golden が解けない");
        assert_eq!(addr, SocketAddr::from(([203, 0, 113, 5], 4242)));
    }

    #[test]
    fn encode_binding_request_has_no_attributes_and_carries_the_given_transaction_id() {
        let txn: TransactionId = [7; TRANSACTION_ID_LEN];
        let buf = encode_binding_request(txn);
        assert_eq!(buf.len(), HEADER_LEN);
        assert_eq!(u16::from_be_bytes([buf[0], buf[1]]), TYPE_BINDING_REQUEST);
        assert_eq!(
            u16::from_be_bytes([buf[2], buf[3]]),
            0,
            "属性なしなら length 0"
        );
        assert_eq!(
            u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            MAGIC_COOKIE
        );
        assert_eq!(&buf[8..20], &txn[..]);
    }

    #[test]
    fn poison_transaction_id_mismatch() {
        let other: TransactionId = [0; TRANSACTION_ID_LEN];
        assert_eq!(
            decode_binding_response(&IPV4_XOR_RESPONSE, &other),
            Err(StunError::TransactionMismatch)
        );
    }

    #[test]
    fn poison_bad_cookie() {
        let mut bytes = IPV4_XOR_RESPONSE;
        bytes[4] ^= 0xFF;
        assert_eq!(
            decode_binding_response(&bytes, &TXN1),
            Err(StunError::BadCookie)
        );
    }

    #[test]
    fn poison_leading_bits_not_zero() {
        let mut bytes = IPV4_XOR_RESPONSE;
        bytes[0] |= 0xC0;
        assert_eq!(
            decode_binding_response(&bytes, &TXN1),
            Err(StunError::BadLeadingBits)
        );
    }

    #[test]
    fn poison_length_not_multiple_of_4() {
        let mut bytes = IPV4_XOR_RESPONSE.to_vec();
        // length を 12 → 13 にし、本体を 1 バイト増やして「本体と合わない」の方を先に踏まないようにする。
        bytes[3] = 13;
        bytes.push(0);
        assert_eq!(
            decode_binding_response(&bytes, &TXN1),
            Err(StunError::LengthNotMultipleOf4)
        );
    }

    #[test]
    fn poison_length_mismatch_with_actual_body() {
        let mut bytes = IPV4_XOR_RESPONSE.to_vec();
        bytes.push(0); // 本体の後ろに 1 バイト余らせる。length は 12 のまま。
        assert_eq!(
            decode_binding_response(&bytes, &TXN1),
            Err(StunError::LengthMismatch)
        );
    }

    #[test]
    fn poison_attr_length_overruns_the_body() {
        let mut bytes = IPV4_XOR_RESPONSE;
        // 「本体と合わない」より先に属性の枠のはみ出しに当たる。
        bytes[23] = 200;
        assert_eq!(
            decode_binding_response(&bytes, &TXN1),
            Err(StunError::AttrOverrun)
        );
    }

    #[test]
    fn poison_unknown_address_family() {
        let mut bytes = IPV4_XOR_RESPONSE;
        bytes[25] = 0x03; // family（0x01/0x02 のどちらでもない）
        assert_eq!(
            decode_binding_response(&bytes, &TXN1),
            Err(StunError::UnknownFamily)
        );
    }

    #[test]
    fn poison_error_response() {
        let mut bytes = IPV4_XOR_RESPONSE;
        bytes[0..2].copy_from_slice(&TYPE_BINDING_ERROR_RESPONSE.to_be_bytes());
        assert_eq!(
            decode_binding_response(&bytes, &TXN1),
            Err(StunError::ErrorResponse)
        );
    }

    #[test]
    fn poison_truncated_below_header_len() {
        assert_eq!(
            decode_binding_response(&IPV4_XOR_RESPONSE[..19], &TXN1),
            Err(StunError::Truncated)
        );
    }

    #[test]
    fn poison_unexpected_type() {
        let mut bytes = IPV4_XOR_RESPONSE;
        bytes[0..2].copy_from_slice(&TYPE_BINDING_REQUEST.to_be_bytes());
        assert_eq!(
            decode_binding_response(&bytes, &TXN1),
            Err(StunError::UnexpectedType(TYPE_BINDING_REQUEST))
        );
    }

    #[test]
    fn poison_no_address_attribute() {
        let mut bytes = IPV4_XOR_RESPONSE[..HEADER_LEN].to_vec();
        bytes[2] = 0;
        bytes[3] = 0;
        assert_eq!(
            decode_binding_response(&bytes, &TXN1),
            Err(StunError::NoAddress)
        );
    }

    #[test]
    fn poison_malformed_address_value_length() {
        // XOR-MAPPED-ADDRESS の value を短くする（length 8 → 4。address 抜き）。
        let mut bytes = IPV4_XOR_RESPONSE[..28].to_vec();
        bytes[3] = 8; // header の length はそのまま（属性ヘッダ 4B + value 4B = 8B）
        bytes[23] = 4; // 属性の length を 4 に
        assert_eq!(
            decode_binding_response(&bytes, &TXN1),
            Err(StunError::MalformedAddress)
        );
    }

    #[test]
    fn unknown_attribute_before_the_address_is_skipped() {
        // 数えない）。パディングを跳ばさずに次を読むと、後ろの XOR-MAPPED-ADDRESS の type が
        let mut bytes = Vec::with_capacity(IPV4_XOR_RESPONSE.len() + 8);
        bytes.extend_from_slice(&IPV4_XOR_RESPONSE[..HEADER_LEN]);
        bytes[2..4].copy_from_slice(&20u16.to_be_bytes()); // 8B（知らない属性。ヘッダ4B+value3B+パディング1B）+ 12B（元の length）
        bytes.extend_from_slice(&[0x99, 0x99, 0x00, 0x03, 0xDE, 0xAD, 0xBE, 0x00]);
        bytes.extend_from_slice(&IPV4_XOR_RESPONSE[HEADER_LEN..]);
        let addr = decode_binding_response(&bytes, &TXN1).expect("知らない属性で止まった");
        assert_eq!(addr, SocketAddr::from(([203, 0, 113, 5], 4242)));
    }

    struct SplitMix64(u64);

    impl SplitMix64 {
        fn new(seed: u64) -> Self {
            SplitMix64(seed)
        }

        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn next_u8(&mut self) -> u8 {
            (self.next_u64() >> 56) as u8
        }
    }

    #[test]
    fn decode_never_panics_on_50k_garbage_inputs() {
        let mut rng = SplitMix64::new(0x5EED_F00D_ABCD_EF01);
        let txn: TransactionId = [0; TRANSACTION_ID_LEN];
        for _ in 0..50_000u32 {
            let len = (rng.next_u64() % 201) as usize; // 0..=200
            let mut bytes = vec![0u8; len];
            for b in bytes.iter_mut() {
                *b = rng.next_u8();
            }
            let _ = decode_binding_response(&bytes, &txn);
        }
    }

    #[test]
    fn decode_never_panics_on_garbage_that_looks_like_a_valid_header() {
        // 上の完全ランダムだと、cookie が一致する確率が 2^32 分の 1 でほぼ通らず、属性の
        let mut rng = SplitMix64::new(0x1234_5678_9ABC_DEF0);
        for _ in 0..50_000u32 {
            let body_len = ((rng.next_u64() % 51) as usize) * 4; // 0, 4, .., 200（4 の倍数）
            let mut bytes = vec![0u8; HEADER_LEN + body_len];
            bytes[0..2].copy_from_slice(&TYPE_BINDING_SUCCESS_RESPONSE.to_be_bytes());
            bytes[2..4].copy_from_slice(&(body_len as u16).to_be_bytes());
            bytes[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            for b in bytes[HEADER_LEN..].iter_mut() {
                *b = rng.next_u8();
            }
            let mut txn = [0u8; TRANSACTION_ID_LEN];
            txn.copy_from_slice(&bytes[8..HEADER_LEN]);
            let _ = decode_binding_response(&bytes, &txn);
        }
    }

    #[test]
    fn encode_binding_success_matches_hand_computed_goldens() {
        let mut out = [0u8; MAX_SUCCESS_LEN];
        let n = encode_binding_success(TXN1, SocketAddr::from(([203, 0, 113, 5], 4242)), &mut out);
        assert_eq!(&out[..n], &IPV4_XOR_RESPONSE[..]);
        let ip: Ipv6Addr = "2001:db8:85a3::8a2e:370:7334".parse().unwrap();
        let n = encode_binding_success(TXN2, SocketAddr::new(IpAddr::V6(ip), 5000), &mut out);
        assert_eq!(&out[..n], &IPV6_XOR_RESPONSE[..]);
    }

    #[test]
    fn binding_request_txn_reads_only_requests() {
        assert_eq!(
            binding_request_txn(&encode_binding_request(TXN1)),
            Some(TXN1)
        );
        assert_eq!(binding_request_txn(&IPV4_XOR_RESPONSE), None);
        assert_eq!(
            binding_request_txn(&encode_binding_request(TXN1)[..19]),
            None
        );
        let mut bad = encode_binding_request(TXN1);
        bad[4] ^= 1;
        assert_eq!(binding_request_txn(&bad), None);
    }

    #[test]
    fn query_resends_every_interval_and_takes_the_matching_answer() {
        let mut q = Query::new(TXN1);
        let mut sent = Vec::new();
        for t in 0..3_000u32 {
            if let Some(req) = q.poll_send(t) {
                assert_eq!(binding_request_txn(&req), Some(TXN1));
                sent.push(t);
            }
        }
        assert_eq!(sent, (0..8).map(|i| i * 250).collect::<Vec<_>>());
        assert_eq!(q.state(), QueryState::TimedOut);

        let mut q = Query::new(TXN1);
        assert!(q.poll_send(0).is_some());
        assert!(!q.on_datagram(&IPV6_XOR_RESPONSE));
        assert_eq!((q.state(), q.ignored), (QueryState::Waiting, 1));
        assert!(q.on_datagram(&IPV4_XOR_RESPONSE));
        assert_eq!(
            q.state(),
            QueryState::Done(SocketAddr::from(([203, 0, 113, 5], 4242)))
        );
        assert!(q.poll_send(250).is_none(), "答えの後も送った");
    }

    #[test]
    fn keep_asks_every_interval_and_counts_only_real_changes() {
        let code = SocketAddr::from(([203, 0, 113, 5], 4242));
        let moved_port = SocketAddr::from(([203, 0, 113, 5], 4250));
        let moved_addr = SocketAddr::from(([198, 51, 100, 9], 4250));
        let mut k = Keep::new(code, 1_000, KEEP_INTERVAL_MS);
        let answer = |txn: TransactionId, a: SocketAddr| {
            let mut out = [0u8; MAX_SUCCESS_LEN];
            let n = encode_binding_success(txn, a, &mut out);
            out[..n].to_vec()
        };
        assert!(!k.due(1_000));
        assert!(!k.due(1_000 + KEEP_INTERVAL_MS - 1));
        assert!(k.due(1_000 + KEEP_INTERVAL_MS));
        let t1 = 1_000 + KEEP_INTERVAL_MS;
        let req = k.send(t1, TXN1);
        assert_eq!(binding_request_txn(&req), Some(TXN1));
        assert!(
            !k.due(t1 + 1) && !k.due(t1 + KEEP_INTERVAL_MS - 1) && k.due(t1 + KEEP_INTERVAL_MS)
        );
        assert_eq!(k.on_datagram(&answer(TXN2, moved_port)), None);
        assert_eq!(k.on_datagram(&IPV4_XOR_RESPONSE[..25]), None);
        assert_eq!(k.counts().ignored, 2);
        // 効く期間: 同じアドレスの答えは使われるが「変わった」には数えない。二重の答えは使わない。
        assert_eq!(k.on_datagram(&answer(TXN1, code)), Some(Remap::Same));
        assert_eq!(
            k.on_datagram(&answer(TXN1, moved_port)),
            None,
            "答えの二重を使った"
        );
        let c = k.counts();
        assert_eq!((c.sent, c.answered, c.ignored), (1, 1, 3));
        assert_eq!(
            (c.changed_port, c.changed_addr, c.differs_from_code),
            (0, 0, false)
        );
        let t2 = t1 + KEEP_INTERVAL_MS;
        k.send(t2, TXN2);
        assert_eq!(
            k.on_datagram(&answer(TXN2, moved_port)),
            Some(Remap::PortOnly)
        );
        let t3 = t2 + KEEP_INTERVAL_MS;
        k.send(t3, TXN1);
        assert_eq!(k.on_datagram(&answer(TXN1, moved_port)), Some(Remap::Same));
        let c = k.counts();
        assert_eq!(
            (c.changed_port, c.changed_addr, c.differs_from_code),
            (1, 0, true)
        );
        let t4 = t3 + KEEP_INTERVAL_MS;
        k.send(t4, TXN2);
        assert_eq!(
            k.on_datagram(&answer(TXN2, moved_addr)),
            Some(Remap::Address)
        );
        let t5 = t4 + KEEP_INTERVAL_MS;
        k.send(t5, TXN1);
        assert_eq!(k.on_datagram(&answer(TXN1, code)), Some(Remap::Address));
        let c = k.counts();
        assert_eq!((c.sent, c.answered), (5, 5));
        assert_eq!(
            (c.changed_port, c.changed_addr, c.differs_from_code),
            (1, 2, false)
        );
        let wrap = Keep::new(code, u32::MAX - 5, 20);
        assert!(!wrap.due(10) && wrap.due(14));
    }

    #[test]
    fn remap_tells_port_from_address() {
        let a = |s: &str| -> SocketAddr { s.parse().unwrap() };
        assert_eq!(remap(a("203.0.113.5:1"), a("203.0.113.5:1")), Remap::Same);
        assert_eq!(
            remap(a("203.0.113.5:1"), a("203.0.113.5:2")),
            Remap::PortOnly
        );
        assert_eq!(
            remap(a("203.0.113.5:1"), a("203.0.113.6:1")),
            Remap::Address
        );
        assert_eq!(
            remap(a("203.0.113.5:1"), a("203.0.113.6:2")),
            Remap::Address
        );
        assert_eq!(
            remap(a("203.0.113.5:1"), a("[2001:db8::5]:1")),
            Remap::Address
        );
    }

    #[test]
    fn compare_mappings_tells_the_four_shapes_and_refuses_mixed_families() {
        let a = |s: &str| -> SocketAddr { s.parse().unwrap() };
        let base = a("203.0.113.5:40000");
        let cases = [
            ("203.0.113.5:40000", Some(Mapping::Same)),
            ("203.0.113.5:40007", Some(Mapping::PortOnly)),
            ("198.51.100.9:40000", Some(Mapping::AddrOnly)),
            ("198.51.100.9:40007", Some(Mapping::Both)),
            ("[2001:db8::5]:40000", None),
            ("[::ffff:203.0.113.5]:40000", None),
        ];
        for (other, want) in cases {
            assert_eq!(compare_mappings(base, a(other)), want, "{other}");
            assert_eq!(compare_mappings(a(other), base), want, "逆順 {other}");
        }
        assert!(!Mapping::Same.depends_on_destination());
        assert!(
            [Mapping::PortOnly, Mapping::AddrOnly, Mapping::Both]
                .iter()
                .all(|m| m.depends_on_destination())
        );
        assert_eq!(
            [
                Mapping::Same,
                Mapping::PortOnly,
                Mapping::AddrOnly,
                Mapping::Both
            ]
            .map(Mapping::port_differs),
            [false, true, false, true]
        );
    }

    #[test]
    fn second_probe_compares_only_a_matching_answer_and_gives_up_without_one() {
        let first = SocketAddr::from(([203, 0, 113, 5], 4242));
        let answer = |txn: TransactionId, a: SocketAddr| {
            let mut out = [0u8; MAX_SUCCESS_LEN];
            let n = encode_binding_success(txn, a, &mut out);
            out[..n].to_vec()
        };
        for (second, want) in [
            (first, Mapping::Same),
            (
                SocketAddr::from(([203, 0, 113, 5], 4250)),
                Mapping::PortOnly,
            ),
            (SocketAddr::from(([198, 51, 100, 9], 4250)), Mapping::Both),
        ] {
            let mut p = SecondProbe::new(TXN1, first);
            assert!(p.poll_send(0).is_some());
            assert_eq!(p.outcome(), None, "答える前に結論を作った");
            assert!(!p.on_datagram(&answer(TXN2, second)));
            assert!(!p.on_datagram(&IPV4_XOR_RESPONSE[..25]));
            assert_eq!((p.ignored(), p.outcome()), (2, None));
            assert!(p.on_datagram(&answer(TXN1, second)));
            assert_eq!(p.outcome(), Some(SecondOutcome::Compared(want)));
            assert!(p.poll_send(250).is_none());
            assert!(!p.on_datagram(&answer(TXN1, SocketAddr::from(([192, 0, 2, 1], 1)))));
            assert_eq!(p.outcome(), Some(SecondOutcome::Compared(want)));
        }
        let v6 = SocketAddr::new(IpAddr::V6("2001:db8::5".parse().unwrap()), 4242);
        let mut p = SecondProbe::new(TXN1, first);
        assert!(p.poll_send(0).is_some());
        assert!(p.on_datagram(&answer(TXN1, v6)));
        assert_eq!(p.outcome(), Some(SecondOutcome::Incomparable));
        let mut p = SecondProbe::new(TXN1, first);
        let mut sent = Vec::new();
        for t in 0..3_000u32 {
            let was_done = p.outcome().is_some();
            if p.poll_send(t).is_some() {
                sent.push(t);
            }
            assert!(!was_done || p.outcome().is_some());
            if t < 8 * 250 {
                assert_eq!(p.outcome(), None, "{t} ms で諦めた");
            }
        }
        assert_eq!(sent, (0..8).map(|i| i * 250).collect::<Vec<_>>());
        assert_eq!(p.outcome(), Some(SecondOutcome::NoAnswer));
    }
}
