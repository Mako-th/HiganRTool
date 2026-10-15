//! LAN 対戦の申し込みのやりとり（pure）。クライアントがホストのアドレスへ参加を申し込み、ホストが始める側の接続コードをそのまま返し、
//! クライアントが返すコードを返す。観戦は申し込みに観戦の印を付け、ホストが観戦コードを返す。ルームサーバーが運ぶ接続コードの代わりに
//! LAN の UDP が運ぶだけで、コードの中身も、そのあとの接続（HELLO・鍵の交換）も、インターネットの経路と同じ。
//!
//! - 1 通 = マジック `HGLN`（4 B）・版（1 B）・種類（1 B）・申し込みの番号（4 B。クライアントが引く乱数で、返事に写す）・種類ごとの本文。
//!   ゲームの受信ソケットとは別のソケットで受ける（ゲームの受信ループには入れない）ので、ゲームの通信の `HGRT` とは混ざらない。
//! - 本文のコードは ASCII の表示できる文字だけ・`code::MAX_CODE_CHARS` まで。長さは 2 B で持ち、1 通の長さと合わなければ捨てる。
//! - 守るのは LAN の中の取り違えと再送だけ（秘密は載せない。コードの中の秘密は LAN の中を平文で渡る）。ホストは先着 1 人（`Host`）。
//!
//! 状態機械 `Host` / `Client` / `Watcher` は時刻を引数で受ける（ms）。送る・開く・待つ副作用は呼び手（dll の `lan_link`）の仕事で、ここは
//! 「何を返すか・次に何をさせるか」だけを決める。

use core::net::SocketAddrV4;

use crate::code::MAX_CODE_CHARS;

pub const MAGIC: [u8; 4] = *b"HGLN";
/// 申し込みのやりとりの版。通信の `PROTO_VER` とは別（このやりとりの形が変わるときだけ上げる）。
pub const LAN_VER: u8 = 1;
const HEADER_LEN: usize = 10;

const K_JOIN: u8 = 1;
const K_OFFER: u8 = 2;
const K_REPLY: u8 = 3;
const K_REPLY_ACK: u8 = 4;
const K_WATCH: u8 = 5;
const K_WATCH_CODE: u8 = 6;
const K_REFUSE: u8 = 7;

/// 申し込みを送り直す間隔（ms）。
pub const RETRY_MS: u64 = 500;
/// クライアントがホストの最初の返事（始める側のコード）を待つ上限（ms）。
pub const CLIENT_WAIT_OFFER_MS: u64 = 10_000;
/// クライアントが返すコードの受け取りの確認を待つ上限（ms）。
pub const CLIENT_WAIT_ACK_MS: u64 = 10_000;
/// 観戦の申し込みの返事を待つ上限（ms）。
pub const WATCH_WAIT_MS: u64 = 10_000;
/// ホストが、参加を受けてから始める側のコードを用意するまでに待つ上限（ms。ネットワークを開く時間）。
pub const HOST_WAIT_OFFER_MS: u64 = 15_000;
/// ホストが、コードを返してから返すコードが届くまでに待つ上限（ms）。過ぎたら待機へ戻る（打ち間違えた 1 通で待機を終えない）。
pub const HOST_WAIT_REPLY_MS: u64 = 30_000;

/// 断る理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// もう別の人が参加している（先着 1 人）。
    Busy,
    /// 観戦の申し込みだが、まだ対戦が始まっていない（観戦コードは対戦がつながってから出せる）。
    NotYet,
    /// 観戦を受けない設定（`[spectate] enable = 0`）か、観戦の用意ができていない。
    WatchOff,
    /// バージョンが違う（`theirs` は断りを受ける側から見た相手＝断る側の通信のバージョン）。
    OtherVersion { theirs: u8 },
    /// ビルドが違う（`theirs` は断る側のビルド番号）。
    OtherBuild { theirs: u32 },
}

impl Why {
    fn code(self) -> (u8, u32) {
        match self {
            Why::Busy => (1, 0),
            Why::NotYet => (2, 0),
            Why::WatchOff => (3, 0),
            Why::OtherVersion { theirs } => (4, u32::from(theirs)),
            Why::OtherBuild { theirs } => (5, theirs),
        }
    }

    fn from_code(c: u8, arg: u32) -> Option<Why> {
        Some(match c {
            1 => Why::Busy,
            2 => Why::NotYet,
            3 => Why::WatchOff,
            4 => Why::OtherVersion {
                theirs: u8::try_from(arg).ok()?,
            },
            5 => Why::OtherBuild { theirs: arg },
            _ => return None,
        })
    }
}

/// 1 通。`code` は借りた文字（`decode` は元のバイト列を借りる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg<'a> {
    Join { id: u32, proto: u8, build: u32 },
    Offer { id: u32, code: &'a str },
    Reply { id: u32, code: &'a str },
    ReplyAck { id: u32 },
    Watch { id: u32, proto: u8, build: u32 },
    WatchCode { id: u32, code: &'a str },
    Refuse { id: u32, why: Why },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    TooShort,
    BadMagic,
    /// このやりとりの版が違う（中は相手の版）。
    BadVersion(u8),
    BadKind(u8),
    /// 本文の長さが種類と合わない。
    BadLength,
    /// コードが ASCII の表示できる文字だけでない、または `MAX_CODE_CHARS` を越える。
    BadText,
    BadReason,
}

/// 受けたバイト列がこのやりとりの 1 通らしいか（マジックだけ見る。ほかの通信を読み捨てるときの振り分け）。
pub fn looks_like_lan(bytes: &[u8]) -> bool {
    bytes.starts_with(&MAGIC)
}

fn header(out: &mut Vec<u8>, kind: u8, id: u32) {
    out.extend_from_slice(&MAGIC);
    out.push(LAN_VER);
    out.push(kind);
    out.extend_from_slice(&id.to_le_bytes());
}

/// 1 通を作る。コードが長すぎる・ASCII の表示できる文字だけでないときは `None`（呼び手が送らない）。
pub fn encode(msg: &Msg<'_>) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(HEADER_LEN + 2 + MAX_CODE_CHARS);
    let text = |out: &mut Vec<u8>, code: &str| -> Option<()> {
        if code.len() > MAX_CODE_CHARS || !code.bytes().all(|b| b.is_ascii_graphic()) {
            return None;
        }
        out.extend_from_slice(&u16::try_from(code.len()).ok()?.to_le_bytes());
        out.extend_from_slice(code.as_bytes());
        Some(())
    };
    match *msg {
        Msg::Join { id, proto, build } => {
            header(&mut out, K_JOIN, id);
            out.push(proto);
            out.extend_from_slice(&build.to_le_bytes());
        }
        Msg::Watch { id, proto, build } => {
            header(&mut out, K_WATCH, id);
            out.push(proto);
            out.extend_from_slice(&build.to_le_bytes());
        }
        Msg::Offer { id, code } => {
            header(&mut out, K_OFFER, id);
            text(&mut out, code)?;
        }
        Msg::Reply { id, code } => {
            header(&mut out, K_REPLY, id);
            text(&mut out, code)?;
        }
        Msg::WatchCode { id, code } => {
            header(&mut out, K_WATCH_CODE, id);
            text(&mut out, code)?;
        }
        Msg::ReplyAck { id } => header(&mut out, K_REPLY_ACK, id),
        Msg::Refuse { id, why } => {
            header(&mut out, K_REFUSE, id);
            let (c, arg) = why.code();
            out.push(c);
            out.extend_from_slice(&arg.to_le_bytes());
        }
    }
    Some(out)
}

/// 1 通を読む。長さは本文と過不足なく合っていなければ捨てる（余りのバイトも断る）。
pub fn decode(bytes: &[u8]) -> Result<Msg<'_>, DecodeError> {
    if bytes.len() < HEADER_LEN {
        return Err(DecodeError::TooShort);
    }
    if !bytes.starts_with(&MAGIC) {
        return Err(DecodeError::BadMagic);
    }
    if bytes[4] != LAN_VER {
        return Err(DecodeError::BadVersion(bytes[4]));
    }
    let kind = bytes[5];
    let id = u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
    let body = &bytes[HEADER_LEN..];
    fn version_body(body: &[u8]) -> Result<(u8, u32), DecodeError> {
        match body {
            [p, b0, b1, b2, b3] => Ok((*p, u32::from_le_bytes([*b0, *b1, *b2, *b3]))),
            _ => Err(DecodeError::BadLength),
        }
    }
    fn text(body: &[u8]) -> Result<&str, DecodeError> {
        let [l0, l1, rest @ ..] = body else {
            return Err(DecodeError::BadLength);
        };
        let len = usize::from(u16::from_le_bytes([*l0, *l1]));
        if rest.len() != len {
            return Err(DecodeError::BadLength);
        }
        if len > MAX_CODE_CHARS || !rest.iter().all(u8::is_ascii_graphic) {
            return Err(DecodeError::BadText);
        }
        core::str::from_utf8(rest).map_err(|_| DecodeError::BadText)
    }
    match kind {
        K_JOIN => {
            let (proto, build) = version_body(body)?;
            Ok(Msg::Join { id, proto, build })
        }
        K_WATCH => {
            let (proto, build) = version_body(body)?;
            Ok(Msg::Watch { id, proto, build })
        }
        K_OFFER => Ok(Msg::Offer {
            id,
            code: text(body)?,
        }),
        K_REPLY => Ok(Msg::Reply {
            id,
            code: text(body)?,
        }),
        K_WATCH_CODE => Ok(Msg::WatchCode {
            id,
            code: text(body)?,
        }),
        K_REPLY_ACK => {
            if body.is_empty() {
                Ok(Msg::ReplyAck { id })
            } else {
                Err(DecodeError::BadLength)
            }
        }
        K_REFUSE => match body {
            [c, a0, a1, a2, a3] => {
                let why = Why::from_code(*c, u32::from_le_bytes([*a0, *a1, *a2, *a3]))
                    .ok_or(DecodeError::BadReason)?;
                Ok(Msg::Refuse { id, why })
            }
            _ => Err(DecodeError::BadLength),
        },
        k => Err(DecodeError::BadKind(k)),
    }
}

/// 自分の通信のバージョンとビルド番号（相手と照らす）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ver {
    pub proto: u8,
    pub build: u32,
}

impl Ver {
    /// 相手（申し込んだ側）の値が違えば断る理由。通信のバージョンを先に見る。理由の `theirs` は断りを受ける側から見た相手、
    /// つまり断る側（自分）の値（受けた側の画面が「相手のバージョンが違います（自分 N・相手 M）」と出せるように）。
    pub fn check(self, proto: u8, build: u32) -> Option<Why> {
        if proto != self.proto {
            Some(Why::OtherVersion { theirs: self.proto })
        } else if build != self.build {
            Some(Why::OtherBuild { theirs: self.build })
        } else {
            None
        }
    }
}

// ---- ホスト ----

/// ホストの状態機械が呼び手にさせること。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAct {
    /// `to` へ送る。
    Send { to: SocketAddrV4, bytes: Vec<u8> },
    /// 最初の参加者を受けた。呼び手は `src` へ届くアドレスでゲームのネットワークを開き、始める側のコードを作って `Host::offer_ready` へ渡す。
    Accepted { src: SocketAddrV4 },
    /// クライアントの返すコードが届いた（1 回だけ）。呼び手はこれを読んで接続を始める。
    DeliverReply { code: String },
    /// 観戦コードを頼まれた（対戦がつながっている間）。呼び手は観戦コードを用意して `Host::watch_code_ready` へ渡す。
    WatchWanted { src: SocketAddrV4, id: u32 },
    /// 返すコードが届かないまま上限が過ぎたので、待機へ戻った（次の参加者を受ける）。
    Rewait,
    /// 始める側のコードが用意できないまま上限が過ぎた（ネットワークを開けない）。
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostState {
    /// 参加者を待っている。
    Waiting,
    /// 参加者を受けた。始める側のコードを待っている。
    Opening {
        src: SocketAddrV4,
        id: u32,
        since: u64,
    },
    /// コードを返した。返すコードを待っている。
    Offered {
        src: SocketAddrV4,
        id: u32,
        code: String,
        since: u64,
    },
    /// 返すコードを渡した（確認を返した）。
    Replied { src: SocketAddrV4, id: u32 },
    /// 対戦がつながった。
    Linked,
    /// 待つ上限が過ぎた。
    Dead,
}

/// ホストの申し込みの受け口（先着 1 人）。
#[derive(Debug, Clone)]
pub struct Host {
    me: Ver,
    state: HostState,
    /// 観戦の申し込みを受ける設定か。
    watch_ok: bool,
    /// 返すコードを待つ上限（ms。既定 `HOST_WAIT_REPLY_MS`）。
    reply_wait: u64,
}

fn send(to: SocketAddrV4, msg: &Msg<'_>) -> Option<HostAct> {
    encode(msg).map(|bytes| HostAct::Send { to, bytes })
}

impl Host {
    pub fn new(me: Ver, watch_ok: bool) -> Host {
        Host {
            me,
            state: HostState::Waiting,
            watch_ok,
            reply_wait: HOST_WAIT_REPLY_MS,
        }
    }

    /// 返すコードを待つ上限を替える（運び手の検査が、実時間で待機へ戻るところを見るため）。
    pub fn with_reply_wait(mut self, ms: u64) -> Host {
        self.reply_wait = ms;
        self
    }

    /// 参加者を受けた後か（このあと来る別の参加の申し込みは断る）。
    pub fn is_taken(&self) -> bool {
        !matches!(self.state, HostState::Waiting)
    }

    /// 受けたバイト列 1 通。壊れたもの・ほかの通信は何もしない。
    pub fn on_datagram(&mut self, src: SocketAddrV4, bytes: &[u8], now: u64) -> Vec<HostAct> {
        let Ok(msg) = decode(bytes) else {
            return Vec::new();
        };
        let mut acts = Vec::new();
        match msg {
            Msg::Join { id, proto, build } => {
                if let Some(why) = self.me.check(proto, build) {
                    acts.extend(send(src, &Msg::Refuse { id, why }));
                    return acts;
                }
                match &self.state {
                    HostState::Waiting => {
                        self.state = HostState::Opening {
                            src,
                            id,
                            since: now,
                        };
                        acts.push(HostAct::Accepted { src });
                    }
                    // 同じ参加者の再送: コードを用意できていれば返し直す（失われた返事の分）。用意中なら何もしない。
                    HostState::Opening { src: s, id: i, .. } if *s == src && *i == id => {}
                    HostState::Offered {
                        src: s,
                        id: i,
                        code,
                        ..
                    } if *s == src && *i == id => {
                        acts.extend(send(src, &Msg::Offer { id, code }));
                    }
                    // 同じアドレスからの新しい番号: 取りやめてやり直した同じ PC（開き直したソケットはポートも替わる）。返すコードを受け取る前なら
                    // 前の申し込みを捨てて置き換える。用意したコードはまだ誰にも読まれていないので、そのまま返す。
                    HostState::Opening { src: s, since, .. } if s.ip() == src.ip() => {
                        self.state = HostState::Opening {
                            src,
                            id,
                            since: *since,
                        };
                    }
                    HostState::Offered { src: s, code, .. } if s.ip() == src.ip() => {
                        acts.extend(send(src, &Msg::Offer { id, code }));
                        self.state = HostState::Offered {
                            src,
                            id,
                            code: code.clone(),
                            since: now,
                        };
                    }
                    _ => acts.extend(send(src, &Msg::Refuse { id, why: Why::Busy })),
                }
            }
            Msg::Reply { id, code } => match &self.state {
                HostState::Offered { src: s, id: i, .. } if *s == src && *i == id => {
                    self.state = HostState::Replied { src, id };
                    acts.push(HostAct::DeliverReply {
                        code: code.to_string(),
                    });
                    acts.extend(send(src, &Msg::ReplyAck { id }));
                }
                // 確認が失われてクライアントが送り直した: 確認だけ返し直す（コードは渡し直さない）。
                HostState::Replied { src: s, id: i } if *s == src && *i == id => {
                    acts.extend(send(src, &Msg::ReplyAck { id }));
                }
                _ => {}
            },
            Msg::Watch { id, proto, build } => {
                if let Some(why) = self.me.check(proto, build) {
                    acts.extend(send(src, &Msg::Refuse { id, why }));
                } else if !self.watch_ok {
                    acts.extend(send(
                        src,
                        &Msg::Refuse {
                            id,
                            why: Why::WatchOff,
                        },
                    ));
                } else if self.state == HostState::Linked {
                    acts.push(HostAct::WatchWanted { src, id });
                } else {
                    acts.extend(send(
                        src,
                        &Msg::Refuse {
                            id,
                            why: Why::NotYet,
                        },
                    ));
                }
            }
            // ホストが受けない種類（クライアント向け）。
            Msg::Offer { .. }
            | Msg::ReplyAck { .. }
            | Msg::WatchCode { .. }
            | Msg::Refuse { .. } => {}
        }
        acts
    }

    /// 始める側のコードを用意できた。参加者へ返す。
    pub fn offer_ready(&mut self, code: &str, now: u64) -> Vec<HostAct> {
        let HostState::Opening { src, id, .. } = self.state else {
            return Vec::new();
        };
        let acts: Vec<HostAct> = send(src, &Msg::Offer { id, code }).into_iter().collect();
        self.state = HostState::Offered {
            src,
            id,
            code: code.to_string(),
            since: now,
        };
        acts
    }

    /// 対戦がつながった（以後、観戦の申し込みを受ける）。
    pub fn linked(&mut self) {
        if !matches!(self.state, HostState::Dead) {
            self.state = HostState::Linked;
        }
    }

    /// 観戦コードを用意できた（`WatchWanted` の答え）。
    pub fn watch_code_ready(&self, src: SocketAddrV4, id: u32, code: &str) -> Vec<HostAct> {
        send(src, &Msg::WatchCode { id, code })
            .into_iter()
            .collect()
    }

    /// 観戦コードを出せなかった（用意が無い・ホストの役でない、など）。
    pub fn watch_refused(&self, src: SocketAddrV4, id: u32, why: Why) -> Vec<HostAct> {
        send(src, &Msg::Refuse { id, why }).into_iter().collect()
    }

    /// 受けた参加者を断って待機へ戻す（呼び手がその相手を受けられないとき。例: ゲームのネットワークが別のアドレスで開いたまま）。
    pub fn refuse_taken(&mut self) -> Vec<HostAct> {
        let HostState::Opening { src, id, .. } = self.state else {
            return Vec::new();
        };
        self.state = HostState::Waiting;
        send(src, &Msg::Refuse { id, why: Why::Busy })
            .into_iter()
            .collect()
    }

    /// 時間が進んだ。待つ上限を過ぎたら一度だけ、コードの用意なら `TimedOut`（以後は何も受けない）、返すコードなら `Rewait`（待機へ戻る）。
    pub fn tick(&mut self, now: u64) -> Vec<HostAct> {
        match &self.state {
            HostState::Opening { since, .. } if now.saturating_sub(*since) > HOST_WAIT_OFFER_MS => {
                self.state = HostState::Dead;
                vec![HostAct::TimedOut]
            }
            HostState::Offered { since, .. } if now.saturating_sub(*since) > self.reply_wait => {
                self.state = HostState::Waiting;
                vec![HostAct::Rewait]
            }
            _ => Vec::new(),
        }
    }
}

// ---- クライアント ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientAct {
    /// ホストへ送る。
    Send(Vec<u8>),
    /// ホストの始める側のコードが届いた（1 回だけ）。呼び手はこれを読んで返すコードを作り、`Client::reply_ready` へ渡す。
    DeliverOffer { code: String },
    /// 返すコードの受け取りを確かめた。
    Done,
    /// ホストが断った。
    Refused(Why),
    /// 待つ上限が過ぎた。
    TimedOut(ClientStage),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientStage {
    /// ホストが返事をしない。
    Offer,
    /// 返すコードの受け取りの確認が来ない。
    Ack,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClientState {
    Joining {
        since: u64,
        last: u64,
    },
    /// コードを呼び手へ渡した。返すコードを待っている。
    Offered,
    Replying {
        code: String,
        since: u64,
        last: u64,
    },
    Done,
    Dead,
}

/// クライアントの参加の手順。
#[derive(Debug, Clone)]
pub struct Client {
    me: Ver,
    id: u32,
    state: ClientState,
}

impl Client {
    /// `id` は申し込みの番号（呼び手が乱数で引く）。作った時刻から待ち始める。最初の 1 通は `start` で。
    pub fn new(me: Ver, id: u32, now: u64) -> Client {
        Client {
            me,
            id,
            state: ClientState::Joining {
                since: now,
                last: now,
            },
        }
    }

    fn join_bytes(&self) -> Option<Vec<u8>> {
        encode(&Msg::Join {
            id: self.id,
            proto: self.me.proto,
            build: self.me.build,
        })
    }

    /// 最初の申し込み。
    pub fn start(&self) -> Vec<ClientAct> {
        self.join_bytes().map(ClientAct::Send).into_iter().collect()
    }

    pub fn on_datagram(&mut self, bytes: &[u8]) -> Vec<ClientAct> {
        let Ok(msg) = decode(bytes) else {
            return Vec::new();
        };
        let mut acts = Vec::new();
        match msg {
            Msg::Offer { id, code } if id == self.id => {
                if matches!(self.state, ClientState::Joining { .. }) {
                    self.state = ClientState::Offered;
                    acts.push(ClientAct::DeliverOffer {
                        code: code.to_string(),
                    });
                }
            }
            Msg::ReplyAck { id } if id == self.id => {
                if matches!(self.state, ClientState::Replying { .. }) {
                    self.state = ClientState::Done;
                    acts.push(ClientAct::Done);
                }
            }
            Msg::Refuse { id, why } if id == self.id => {
                if matches!(
                    self.state,
                    ClientState::Joining { .. }
                        | ClientState::Offered
                        | ClientState::Replying { .. }
                ) {
                    self.state = ClientState::Dead;
                    acts.push(ClientAct::Refused(why));
                }
            }
            _ => {}
        }
        acts
    }

    /// 返すコードを作れた。ホストへ送る（確認が来るまで送り直す）。
    pub fn reply_ready(&mut self, code: &str, now: u64) -> Vec<ClientAct> {
        if !matches!(self.state, ClientState::Offered) {
            return Vec::new();
        }
        let bytes = encode(&Msg::Reply { id: self.id, code });
        self.state = ClientState::Replying {
            code: code.to_string(),
            since: now,
            last: now,
        };
        bytes.map(ClientAct::Send).into_iter().collect()
    }

    pub fn tick(&mut self, now: u64) -> Vec<ClientAct> {
        let mut acts = Vec::new();
        match &mut self.state {
            ClientState::Joining { since, last } => {
                if now.saturating_sub(*since) > CLIENT_WAIT_OFFER_MS {
                    self.state = ClientState::Dead;
                    acts.push(ClientAct::TimedOut(ClientStage::Offer));
                } else if now.saturating_sub(*last) >= RETRY_MS {
                    *last = now;
                    let id = self.id;
                    let me = self.me;
                    acts.extend(
                        encode(&Msg::Join {
                            id,
                            proto: me.proto,
                            build: me.build,
                        })
                        .map(ClientAct::Send),
                    );
                }
            }
            ClientState::Replying { code, since, last } => {
                if now.saturating_sub(*since) > CLIENT_WAIT_ACK_MS {
                    self.state = ClientState::Dead;
                    acts.push(ClientAct::TimedOut(ClientStage::Ack));
                } else if now.saturating_sub(*last) >= RETRY_MS {
                    *last = now;
                    acts.extend(encode(&Msg::Reply { id: self.id, code }).map(ClientAct::Send));
                }
            }
            ClientState::Offered | ClientState::Done | ClientState::Dead => {}
        }
        acts
    }
}

// ---- 観戦 ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatcherAct {
    Send(Vec<u8>),
    /// 観戦コードが届いた（1 回だけ）。
    DeliverCode {
        code: String,
    },
    Refused(Why),
    TimedOut,
}

/// 観戦の申し込みの手順（ホストへ頼んで観戦コードを受け取るまで）。
#[derive(Debug, Clone)]
pub struct Watcher {
    me: Ver,
    id: u32,
    since: u64,
    last: u64,
    finished: bool,
}

impl Watcher {
    pub fn new(me: Ver, id: u32, now: u64) -> Watcher {
        Watcher {
            me,
            id,
            since: now,
            last: now,
            finished: false,
        }
    }

    fn bytes(&self) -> Option<Vec<u8>> {
        encode(&Msg::Watch {
            id: self.id,
            proto: self.me.proto,
            build: self.me.build,
        })
    }

    pub fn start(&self) -> Vec<WatcherAct> {
        self.bytes().map(WatcherAct::Send).into_iter().collect()
    }

    pub fn on_datagram(&mut self, bytes: &[u8]) -> Vec<WatcherAct> {
        if self.finished {
            return Vec::new();
        }
        match decode(bytes) {
            Ok(Msg::WatchCode { id, code }) if id == self.id => {
                self.finished = true;
                vec![WatcherAct::DeliverCode {
                    code: code.to_string(),
                }]
            }
            Ok(Msg::Refuse { id, why }) if id == self.id => {
                self.finished = true;
                vec![WatcherAct::Refused(why)]
            }
            _ => Vec::new(),
        }
    }

    pub fn tick(&mut self, now: u64) -> Vec<WatcherAct> {
        if self.finished {
            return Vec::new();
        }
        if now.saturating_sub(self.since) > WATCH_WAIT_MS {
            self.finished = true;
            return vec![WatcherAct::TimedOut];
        }
        if now.saturating_sub(self.last) >= RETRY_MS {
            self.last = now;
            return self.bytes().map(WatcherAct::Send).into_iter().collect();
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: Ver = Ver { proto: 7, build: 0 };

    fn addr(last: u8, port: u16) -> SocketAddrV4 {
        SocketAddrV4::new(core::net::Ipv4Addr::new(192, 168, 7, last), port)
    }

    fn code(c: char, n: usize) -> String {
        core::iter::repeat_n(c, n).collect()
    }

    fn msgs() -> Vec<Msg<'static>> {
        vec![
            Msg::Join {
                id: 0xDEAD_BEEF,
                proto: 7,
                build: 411_272,
            },
            Msg::Watch {
                id: 1,
                proto: 7,
                build: 0,
            },
            Msg::Offer {
                id: 2,
                code: "AbC-_123",
            },
            Msg::Reply { id: 3, code: "z" },
            Msg::WatchCode { id: 4, code: "Q9~" },
            Msg::ReplyAck { id: u32::MAX },
            Msg::Refuse {
                id: 5,
                why: Why::Busy,
            },
            Msg::Refuse {
                id: 6,
                why: Why::NotYet,
            },
            Msg::Refuse {
                id: 7,
                why: Why::WatchOff,
            },
            Msg::Refuse {
                id: 8,
                why: Why::OtherVersion { theirs: 6 },
            },
            Msg::Refuse {
                id: 9,
                why: Why::OtherBuild { theirs: 123_456 },
            },
        ]
    }

    #[test]
    fn every_message_round_trips() {
        for m in msgs() {
            let b = encode(&m).unwrap();
            assert_eq!(decode(&b), Ok(m), "{m:?}");
            assert!(looks_like_lan(&b));
        }
        // 長さの上限ちょうどのコード。
        let long = code('A', MAX_CODE_CHARS);
        let m = Msg::Offer { id: 1, code: &long };
        let b = encode(&m).unwrap();
        assert_eq!(decode(&b), Ok(m));
        assert!(b.len() <= crate::wire::MAX_DATAGRAM, "{}", b.len());
    }

    /// 壊れたら落ちる: 切れた 1 通・余りのある 1 通・種類や版や理由の壊れた 1 通を、読んでしまう。
    #[test]
    fn truncated_padded_and_bad_messages_are_refused() {
        for m in msgs() {
            let b = encode(&m).unwrap();
            for cut in 0..b.len() {
                assert!(decode(&b[..cut]).is_err(), "{m:?} を {cut} B で切った");
            }
            // 0 は文字の検査でも落ちるので、表示できる文字（長さの検査だけが止める）でも足す。
            for pad in [0u8, b'A'] {
                let mut more = b.clone();
                more.push(pad);
                assert!(decode(&more).is_err(), "{m:?} に {pad} を 1 B 足した");
            }
        }
        let ok = encode(&Msg::ReplyAck { id: 1 }).unwrap();
        let mut bad = ok.clone();
        bad[0] = b'X';
        assert_eq!(decode(&bad), Err(DecodeError::BadMagic));
        let mut bad = ok.clone();
        bad[4] = LAN_VER + 1;
        assert_eq!(decode(&bad), Err(DecodeError::BadVersion(LAN_VER + 1)));
        let mut bad = ok.clone();
        bad[5] = 99;
        assert_eq!(decode(&bad), Err(DecodeError::BadKind(99)));
        let mut refuse = encode(&Msg::Refuse {
            id: 1,
            why: Why::Busy,
        })
        .unwrap();
        refuse[HEADER_LEN] = 77;
        assert_eq!(decode(&refuse), Err(DecodeError::BadReason));
        // 通信のバージョンが u8 に収まらない理由の値。
        let mut refuse = encode(&Msg::Refuse {
            id: 1,
            why: Why::OtherVersion { theirs: 1 },
        })
        .unwrap();
        refuse[HEADER_LEN + 1 + 1] = 1;
        assert_eq!(decode(&refuse), Err(DecodeError::BadReason));
        // ゲームの通信は読まない。
        assert!(!looks_like_lan(b"HGRT\x07\x01"));
    }

    #[test]
    fn code_text_must_be_short_printable_ascii() {
        assert!(
            encode(&Msg::Offer {
                id: 1,
                code: &code('A', MAX_CODE_CHARS + 1)
            })
            .is_none()
        );
        for bad in ["a b", "日本語", "tab\there", "line\nfeed", "nul\0"] {
            assert!(
                encode(&Msg::Reply { id: 1, code: bad }).is_none(),
                "{bad:?}"
            );
        }
        // 受ける側: 長さの欄と本文が合わない・上限を越える・表示できない文字。
        let mut b = encode(&Msg::Offer { id: 1, code: "abc" }).unwrap();
        b[HEADER_LEN] = 4;
        assert_eq!(decode(&b), Err(DecodeError::BadLength));
        let mut b = encode(&Msg::Offer { id: 1, code: "abc" }).unwrap();
        b[HEADER_LEN + 2] = b' ';
        assert_eq!(decode(&b), Err(DecodeError::BadText));
        let mut b = encode(&Msg::Offer { id: 1, code: "abc" }).unwrap();
        b[HEADER_LEN + 2] = 0xC3;
        assert_eq!(decode(&b), Err(DecodeError::BadText));
        let big = {
            let mut v = Vec::new();
            header(&mut v, K_OFFER, 1);
            let n = MAX_CODE_CHARS + 1;
            v.extend_from_slice(&u16::try_from(n).unwrap().to_le_bytes());
            v.extend(core::iter::repeat_n(b'A', n));
            v
        };
        assert_eq!(decode(&big), Err(DecodeError::BadText));
    }

    fn join(id: u32) -> Vec<u8> {
        encode(&Msg::Join {
            id,
            proto: ME.proto,
            build: ME.build,
        })
        .unwrap()
    }

    fn sent(acts: &[HostAct]) -> Vec<(SocketAddrV4, Msg<'_>)> {
        acts.iter()
            .filter_map(|a| match a {
                HostAct::Send { to, bytes } => Some((*to, decode(bytes).unwrap())),
                _ => None,
            })
            .collect()
    }

    /// 壊れたら落ちる: 先着 1 人でなくなる、2 人目に断りを返さない、同じ人の再送で状態や返事が崩れる。
    #[test]
    fn the_host_takes_the_first_joiner_only_and_repeats_its_answers() {
        let mut h = Host::new(ME, true);
        let (a, b) = (addr(10, 50000), addr(11, 50001));
        assert!(!h.is_taken());
        assert_eq!(
            h.on_datagram(a, &join(100), 0),
            vec![HostAct::Accepted { src: a }]
        );
        assert!(h.is_taken());
        // 用意中の再送は何も返さない。
        assert!(h.on_datagram(a, &join(100), 500).is_empty());
        // 2 人目は断る。
        let acts = h.on_datagram(b, &join(200), 600);
        assert_eq!(
            sent(&acts),
            vec![(
                b,
                Msg::Refuse {
                    id: 200,
                    why: Why::Busy
                }
            )]
        );
        // コードを返す。
        let acts = h.offer_ready("OFFERCODE", 800);
        assert_eq!(
            sent(&acts),
            vec![(
                a,
                Msg::Offer {
                    id: 100,
                    code: "OFFERCODE"
                }
            )]
        );
        // 失われた返事の分、再送に同じコードを返す。
        let acts = h.on_datagram(a, &join(100), 1300);
        assert_eq!(
            sent(&acts),
            vec![(
                a,
                Msg::Offer {
                    id: 100,
                    code: "OFFERCODE"
                }
            )]
        );
    }

    #[test]
    fn a_reply_is_delivered_once_and_only_from_the_joiner() {
        let mut h = Host::new(ME, true);
        let (a, b) = (addr(10, 50000), addr(11, 50001));
        h.on_datagram(a, &join(100), 0);
        h.offer_ready("OFFERCODE", 10);
        let reply = |id| {
            encode(&Msg::Reply {
                id,
                code: "REPLYCODE",
            })
            .unwrap()
        };
        // 別のアドレス・別の番号の返すコードは読まない。
        assert!(h.on_datagram(b, &reply(100), 20).is_empty());
        assert!(h.on_datagram(a, &reply(999), 20).is_empty());
        let acts = h.on_datagram(a, &reply(100), 30);
        assert_eq!(
            acts.iter()
                .filter(|a| matches!(a, HostAct::DeliverReply { .. }))
                .count(),
            1
        );
        assert!(acts.contains(&HostAct::DeliverReply {
            code: "REPLYCODE".to_string()
        }));
        assert_eq!(sent(&acts), vec![(a, Msg::ReplyAck { id: 100 })]);
        // 確認が失われて送り直されても、渡し直さず確認だけ返す。
        let acts = h.on_datagram(a, &reply(100), 530);
        assert!(
            !acts
                .iter()
                .any(|a| matches!(a, HostAct::DeliverReply { .. }))
        );
        assert_eq!(sent(&acts), vec![(a, Msg::ReplyAck { id: 100 })]);
    }

    #[test]
    fn a_different_version_or_build_is_refused_before_anything_starts() {
        let mut h = Host::new(
            Ver {
                proto: 7,
                build: 500,
            },
            true,
        );
        let a = addr(10, 50000);
        let old = encode(&Msg::Join {
            id: 1,
            proto: 6,
            build: 500,
        })
        .unwrap();
        assert_eq!(
            sent(&h.on_datagram(a, &old, 0)),
            vec![(
                a,
                Msg::Refuse {
                    id: 1,
                    why: Why::OtherVersion { theirs: 7 }
                }
            )]
        );
        let other = encode(&Msg::Join {
            id: 2,
            proto: 7,
            build: 499,
        })
        .unwrap();
        assert_eq!(
            sent(&h.on_datagram(a, &other, 0)),
            vec![(
                a,
                Msg::Refuse {
                    id: 2,
                    why: Why::OtherBuild { theirs: 500 }
                }
            )]
        );
        assert!(!h.is_taken(), "断った申し込みで先着を取られた");
        // 観戦の申し込みも同じ。
        let w = encode(&Msg::Watch {
            id: 3,
            proto: 6,
            build: 500,
        })
        .unwrap();
        assert!(matches!(
            sent(&h.on_datagram(a, &w, 0))[0].1,
            Msg::Refuse {
                why: Why::OtherVersion { .. },
                ..
            }
        ));
    }

    #[test]
    fn watch_requests_wait_for_a_linked_match_and_follow_the_setting() {
        let a = addr(20, 50100);
        let watch = |id| {
            encode(&Msg::Watch {
                id,
                proto: ME.proto,
                build: ME.build,
            })
            .unwrap()
        };
        let mut h = Host::new(ME, true);
        // 対戦がつながる前: まだ。
        assert_eq!(
            sent(&h.on_datagram(a, &watch(1), 0)),
            vec![(
                a,
                Msg::Refuse {
                    id: 1,
                    why: Why::NotYet
                }
            )]
        );
        h.on_datagram(addr(10, 5), &join(7), 0);
        h.offer_ready("O", 1);
        assert_eq!(
            sent(&h.on_datagram(a, &watch(2), 2)),
            vec![(
                a,
                Msg::Refuse {
                    id: 2,
                    why: Why::NotYet
                }
            )],
            "コードの受け渡しの途中もまだ"
        );
        h.linked();
        assert_eq!(
            h.on_datagram(a, &watch(3), 3),
            vec![HostAct::WatchWanted { src: a, id: 3 }]
        );
        // 何人でも頼める（上限は観戦の側が見る）。
        assert_eq!(
            h.on_datagram(addr(21, 1), &watch(4), 4),
            vec![HostAct::WatchWanted {
                src: addr(21, 1),
                id: 4
            }]
        );
        assert_eq!(
            sent(&h.watch_code_ready(a, 3, "WCODE")),
            vec![(
                a,
                Msg::WatchCode {
                    id: 3,
                    code: "WCODE"
                }
            )]
        );
        assert_eq!(
            sent(&h.watch_refused(a, 3, Why::WatchOff)),
            vec![(
                a,
                Msg::Refuse {
                    id: 3,
                    why: Why::WatchOff
                }
            )]
        );
        // 観戦を受けない設定。
        let mut off = Host::new(ME, false);
        off.linked();
        assert_eq!(
            sent(&off.on_datagram(a, &watch(5), 0)),
            vec![(
                a,
                Msg::Refuse {
                    id: 5,
                    why: Why::WatchOff
                }
            )]
        );
        // つながった後の参加の申し込みは断る。
        assert!(matches!(
            sent(&h.on_datagram(addr(30, 1), &join(77), 9))[0].1,
            Msg::Refuse { why: Why::Busy, .. }
        ));
    }

    /// 壊れたら落ちる: 同じ PC がやり直した申し込み（同じアドレス・別のポートと番号）を断る、置き換えた後に前の申し込みの返すコードを読む、
    /// 返すコードを受け取った後や別のアドレスの申し込みまで置き換える。
    #[test]
    fn a_new_request_from_the_same_address_replaces_the_unanswered_one() {
        let mut h = Host::new(ME, true);
        let (old, again) = (addr(10, 50000), addr(10, 50007));
        let reply = |id| encode(&Msg::Reply { id, code: "R" }).unwrap();
        h.on_datagram(old, &join(100), 0);
        // 用意中に置き換わる: 返事は無く、コードは新しい申し込みへ返る。
        assert!(h.on_datagram(again, &join(200), 10).is_empty());
        assert_eq!(
            sent(&h.offer_ready("OFFER", 20)),
            vec![(
                again,
                Msg::Offer {
                    id: 200,
                    code: "OFFER"
                }
            )]
        );
        // コードを返した後も置き換わる: 同じコードを新しい申し込みへ返し、上限の時計も新しい申し込みから数える。
        let third = addr(10, 50011);
        assert_eq!(
            sent(&h.on_datagram(third, &join(300), 20_000)),
            vec![(
                third,
                Msg::Offer {
                    id: 300,
                    code: "OFFER"
                }
            )]
        );
        assert!(h.tick(20_000 + HOST_WAIT_REPLY_MS).is_empty());
        // 置き換えられた申し込みの返すコードは読まない。
        assert!(h.on_datagram(again, &reply(200), 20_100).is_empty());
        // 別のアドレスは置き換えない。
        assert!(matches!(
            sent(&h.on_datagram(addr(11, 50000), &join(400), 20_200))[0].1,
            Msg::Refuse { why: Why::Busy, .. }
        ));
        // 返すコードを受け取った後は、同じアドレスでも断る。
        assert!(
            h.on_datagram(third, &reply(300), 20_300)
                .contains(&HostAct::DeliverReply {
                    code: "R".to_string()
                })
        );
        assert!(matches!(
            sent(&h.on_datagram(addr(10, 50013), &join(500), 20_400))[0].1,
            Msg::Refuse { why: Why::Busy, .. }
        ));
    }

    /// 壊れたら落ちる: コードの用意の時間切れを言わない、返すコードの時間切れで待機へ戻らない（打ち間違えた 1 通で待機が終わる）、待機へ戻った後に
    /// 次の参加者を受けない。
    #[test]
    fn the_host_gives_up_when_the_code_or_the_reply_does_not_come() {
        let a = addr(10, 50000);
        let mut h = Host::new(ME, true);
        h.on_datagram(a, &join(1), 0);
        assert!(h.tick(HOST_WAIT_OFFER_MS).is_empty());
        assert_eq!(h.tick(HOST_WAIT_OFFER_MS + 1), vec![HostAct::TimedOut]);
        assert!(h.tick(HOST_WAIT_OFFER_MS + 2).is_empty(), "2 回言わない");
        let mut h = Host::new(ME, true);
        h.on_datagram(a, &join(1), 0);
        h.offer_ready("O", 100);
        assert!(h.tick(100 + HOST_WAIT_REPLY_MS).is_empty());
        assert_eq!(h.tick(100 + HOST_WAIT_REPLY_MS + 1), vec![HostAct::Rewait]);
        assert!(!h.is_taken(), "待機へ戻っていない");
        assert!(
            h.tick(100 + HOST_WAIT_REPLY_MS + 2).is_empty(),
            "2 回言わない"
        );
        // 遅れて届いた前の参加者の返すコードは読まない。次の参加者は受ける。
        assert!(
            h.on_datagram(
                a,
                &encode(&Msg::Reply { id: 1, code: "R" }).unwrap(),
                40_000
            )
            .is_empty()
        );
        let b = addr(11, 50000);
        assert_eq!(
            h.on_datagram(b, &join(2), 40_001),
            vec![HostAct::Accepted { src: b }]
        );
        // 受けた参加者を呼び手が受けられないとき: 断って待機へ戻す。
        assert_eq!(
            sent(&h.refuse_taken()),
            vec![(
                b,
                Msg::Refuse {
                    id: 2,
                    why: Why::Busy
                }
            )]
        );
        assert!(!h.is_taken());
        assert!(h.refuse_taken().is_empty(), "待機中は何も断らない");
        // 返すコードが届いた後は時間切れにならない。
        let mut h = Host::new(ME, true);
        h.on_datagram(a, &join(1), 0);
        h.offer_ready("O", 0);
        h.on_datagram(a, &encode(&Msg::Reply { id: 1, code: "R" }).unwrap(), 5);
        assert!(h.tick(10_000_000).is_empty());
    }

    #[test]
    fn garbage_and_foreign_traffic_do_nothing_to_the_host() {
        let mut h = Host::new(ME, true);
        let a = addr(10, 1);
        for junk in [
            &b""[..],
            b"HGRT\x07\x01abcdef",
            b"HGLN",
            b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
        ] {
            assert!(h.on_datagram(a, junk, 0).is_empty());
        }
        assert!(!h.is_taken());
        // クライアント向けの種類をホストが受けても何もしない。
        for m in [
            Msg::Offer { id: 1, code: "x" },
            Msg::ReplyAck { id: 1 },
            Msg::WatchCode { id: 1, code: "x" },
            Msg::Refuse {
                id: 1,
                why: Why::Busy,
            },
        ] {
            assert!(
                h.on_datagram(a, &encode(&m).unwrap(), 0).is_empty(),
                "{m:?}"
            );
        }
    }

    /// 壊れたら落ちる: クライアントの手順（申し込みの再送・コードの受け取り・返すコードの再送と確認・断り・時間切れ）が崩れる。
    #[test]
    fn the_client_repeats_join_until_the_offer_comes_then_repeats_reply_until_acked() {
        let mut c = Client::new(ME, 42, 1000);
        let first = c.start();
        assert_eq!(first, vec![ClientAct::Send(join(42))]);
        assert!(c.tick(1000 + RETRY_MS - 1).is_empty());
        assert_eq!(c.tick(1000 + RETRY_MS), vec![ClientAct::Send(join(42))]);
        // 別の番号の返事は読まない。
        let wrong = encode(&Msg::Offer { id: 43, code: "NO" }).unwrap();
        assert!(c.on_datagram(&wrong).is_empty());
        let offer = encode(&Msg::Offer {
            id: 42,
            code: "OFFER",
        })
        .unwrap();
        assert_eq!(
            c.on_datagram(&offer),
            vec![ClientAct::DeliverOffer {
                code: "OFFER".to_string()
            }]
        );
        // 返事の再送は 2 回渡さない。
        assert!(c.on_datagram(&offer).is_empty());
        // 申し込みはもう送り直さない。
        assert!(c.tick(5000).is_empty());
        let reply = encode(&Msg::Reply {
            id: 42,
            code: "REPLY",
        })
        .unwrap();
        assert_eq!(
            c.reply_ready("REPLY", 6000),
            vec![ClientAct::Send(reply.clone())]
        );
        assert!(c.tick(6000 + RETRY_MS - 1).is_empty());
        assert_eq!(c.tick(6000 + RETRY_MS), vec![ClientAct::Send(reply)]);
        let ack = encode(&Msg::ReplyAck { id: 42 }).unwrap();
        assert_eq!(c.on_datagram(&ack), vec![ClientAct::Done]);
        assert!(c.tick(60_000).is_empty(), "確認の後は何も送らない");
    }

    #[test]
    fn the_client_stops_on_a_refusal_or_a_silent_host() {
        let mut c = Client::new(ME, 1, 0);
        let refuse = encode(&Msg::Refuse {
            id: 1,
            why: Why::Busy,
        })
        .unwrap();
        assert_eq!(c.on_datagram(&refuse), vec![ClientAct::Refused(Why::Busy)]);
        assert!(c.tick(100_000).is_empty());
        let mut c = Client::new(ME, 1, 0);
        assert!(
            c.tick(CLIENT_WAIT_OFFER_MS)
                .iter()
                .all(|a| matches!(a, ClientAct::Send(_)))
        );
        assert_eq!(
            c.tick(CLIENT_WAIT_OFFER_MS + 1),
            vec![ClientAct::TimedOut(ClientStage::Offer)]
        );
        assert!(c.tick(CLIENT_WAIT_OFFER_MS + 2).is_empty());
        // 確認が来ない。
        let mut c = Client::new(ME, 1, 0);
        c.on_datagram(&encode(&Msg::Offer { id: 1, code: "O" }).unwrap());
        c.reply_ready("R", 2);
        assert_eq!(
            c.tick(2 + CLIENT_WAIT_ACK_MS + 1),
            vec![ClientAct::TimedOut(ClientStage::Ack)]
        );
        // 返す前に返すコードを渡されても何も送らない（コードを受けていない）。
        let mut c = Client::new(ME, 1, 0);
        assert!(c.reply_ready("R", 0).is_empty());
    }

    #[test]
    fn the_watcher_asks_until_the_code_or_a_refusal_comes() {
        let mut w = Watcher::new(ME, 9, 0);
        let ask = encode(&Msg::Watch {
            id: 9,
            proto: ME.proto,
            build: ME.build,
        })
        .unwrap();
        assert_eq!(w.start(), vec![WatcherAct::Send(ask.clone())]);
        assert_eq!(w.tick(RETRY_MS), vec![WatcherAct::Send(ask)]);
        let code = encode(&Msg::WatchCode { id: 9, code: "WC" }).unwrap();
        assert_eq!(
            w.on_datagram(&code),
            vec![WatcherAct::DeliverCode {
                code: "WC".to_string()
            }]
        );
        assert!(w.on_datagram(&code).is_empty());
        assert!(w.tick(100_000).is_empty());
        let mut w = Watcher::new(ME, 9, 0);
        let refuse = encode(&Msg::Refuse {
            id: 9,
            why: Why::NotYet,
        })
        .unwrap();
        assert_eq!(
            w.on_datagram(&refuse),
            vec![WatcherAct::Refused(Why::NotYet)]
        );
        let mut w = Watcher::new(ME, 9, 0);
        assert_eq!(w.tick(WATCH_WAIT_MS + 1), vec![WatcherAct::TimedOut]);
        // 別の番号の返事は読まない。
        let mut w = Watcher::new(ME, 9, 0);
        assert!(
            w.on_datagram(&encode(&Msg::WatchCode { id: 8, code: "X" }).unwrap())
                .is_empty()
        );
    }
}
