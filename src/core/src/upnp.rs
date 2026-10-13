//! UPnP の IGD（家のルーター）にポートの割り当てを頼むときの、文面の読み書き（pure。通信は dll の `upnp.rs`）。
//!
//! 流れ: SSDP の M-SEARCH を LAN へ送る → 返事の LOCATION から機器の記述（XML）を取る → その中の WANIPConnection か
//! WANPPPConnection の制御 URL へ SOAP を送る（AddPortMapping / GetSpecificPortMappingEntry / DeletePortMapping）。
//! ここは流れの各段階の文面を作り、返事を読むだけ。
//!
//! 外へ出さないための守り: 読む URL は `http://IPv4のアドレス[:ポート]/…` だけで、アドレスは LAN の中（private / link-local / loopback）。
//! 名前は引かない。LOCATION が外のアドレスを指していても、制御 URL が別のアドレスを指していても断る（LAN の機器の返事に
//! 釣られて外へ送らないため）。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::guard;

pub const SSDP_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
pub const SSDP_PORT: u16 = 1900;
/// 探す機器の型。実機（MiniUPnPd）は :1 と :2 の両方に答えた。
pub const SEARCH_TARGETS: [&str; 2] = [
    "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
    "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
];
/// 割り当ての説明（ルーターの管理画面に出る）。
pub const MAPPING_DESCRIPTION: &str = "HiganRTool";
/// 読む返事の上限（B）。機器の記述は 数 KB。
pub const MAX_RESPONSE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpnpError {
    /// 文面が読めない（中は理由の固定の語）。
    Malformed(&'static str),
    /// 読まない・送らない URL（LAN の外・名前・http でない）。
    Refused(&'static str),
    /// 機器が断った（HTTP の状態と、SOAP の errorCode が読めればその数）。
    Fault { status: u16, code: Option<u32> },
    /// 通信の誤り（つなげない・時間切れ。中は OS の誤りの文）。
    Io(String),
}

impl UpnpError {
    /// ログに出す文（アドレスは書かない）。
    pub fn text(&self) -> String {
        match self {
            UpnpError::Malformed(w) => format!("返事を読めない（{w}）"),
            UpnpError::Refused(w) => format!("使わない URL（{w}）"),
            UpnpError::Fault {
                status,
                code: Some(c),
            } => format!("機器が断った（HTTP {status}・errorCode {c}）"),
            UpnpError::Fault { status, code: None } => {
                format!("機器が断った（HTTP {status}・errorCode なし）")
            }
            UpnpError::Io(e) => format!("通信の誤り（{e}）"),
        }
    }

    /// 機器の errorCode（断られたときだけ）。
    pub fn fault_code(&self) -> Option<u32> {
        match self {
            UpnpError::Fault { code, .. } => *code,
            _ => None,
        }
    }
}

/// 外側のポートがほかの割り当てと重なった（別のポートで頼み直せる）。
pub const FAULT_CONFLICT: u32 = 718;
/// 指定の割り当てが無い（Get / Delete で、消えているのを確かめるときの答え）。
pub const FAULT_NO_SUCH_ENTRY: u32 = 714;
/// 期限なしの割り当てしか受けない（期限つきで頼んだら断られた）。
pub const FAULT_ONLY_PERMANENT: u32 = 725;

// ---------------- URL ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub host: Ipv4Addr,
    pub port: u16,
    /// `/` で始まる経路（問いを含む）。
    pub path: String,
}

/// 空白と制御文字（改行を含む）を含む文字列は HTTP の要求行・Host にコピーできない。
fn check_plain(s: &str) -> Result<(), UpnpError> {
    if s.chars().any(|c| c.is_control() || c == ' ') {
        return Err(UpnpError::Malformed("URL に空白か制御文字がある"));
    }
    Ok(())
}

impl Url {
    /// `http://IPv4[:ポート]/道` だけを読む。名前・https・LAN の外のアドレスは断る。
    /// アドレスは `guard::is_home_v4`（loopback も通す。検査の偽の機器が 127.0.0.1 で待つため。実際の機器の返事の送り手は LAN のアドレス）。
    pub fn parse(s: &str) -> Result<Url, UpnpError> {
        let s = s.trim();
        let head = s
            .get(..7)
            .ok_or(UpnpError::Malformed("URL が短い"))?
            .to_ascii_lowercase();
        if head != "http://" {
            return Err(UpnpError::Refused("http でない"));
        }
        let rest = s.get(7..).unwrap_or("");
        check_plain(rest)?;
        let (auth, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match auth.rsplit_once(':') {
            Some((h, p)) => (
                h,
                p.parse::<u16>()
                    .ok()
                    .filter(|&n| n != 0)
                    .ok_or(UpnpError::Malformed("URL のポートが読めない"))?,
            ),
            None => (auth, 80),
        };
        let host: Ipv4Addr = host
            .parse()
            .map_err(|_| UpnpError::Refused("アドレスが IPv4 の数字でない（名前は引かない）"))?;
        if !guard::is_home_v4(host) {
            return Err(UpnpError::Refused("LAN の外のアドレス"));
        }
        Ok(Url {
            host,
            port,
            path: path.to_string(),
        })
    }

    /// `Host` の欄の値。
    pub fn host_header(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// 相対の指定 `rel`（絶対 URL・`/` で始まる経路・今の経路の隣）をこの URL を基に解く。解いた先のアドレスがこの URL と違えば断る。
    pub fn join(&self, rel: &str) -> Result<Url, UpnpError> {
        let rel = decode_entities(rel.trim());
        let url = if rel
            .get(..7)
            .is_some_and(|h| h.eq_ignore_ascii_case("http://"))
        {
            Url::parse(&rel)?
        } else if rel.starts_with('/') {
            check_plain(&rel)?;
            Url {
                path: rel,
                ..self.clone()
            }
        } else {
            check_plain(&rel)?;
            let dir = match self.path.rfind('/') {
                Some(i) => &self.path[..=i],
                None => "/",
            };
            Url {
                path: format!("{dir}{rel}"),
                ..self.clone()
            }
        };
        if url.host != self.host {
            return Err(UpnpError::Refused("機器と別のアドレスを指している"));
        }
        Ok(url)
    }
}

// ---------------- SSDP ----------------

/// M-SEARCH の本文。`MX` は 1 秒（返事は 0〜MX 秒のうちのどこかで来る）。
pub fn search_request(target: &str) -> Vec<u8> {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_ADDR}:{SSDP_PORT}\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {target}\r\n\r\n"
    )
    .into_bytes()
}

/// M-SEARCH への返事 1 通から機器の記述の URL を読む。`HTTP/1.1 200` で、LOCATION が LAN の中の IPv4 の http だけ通す。
pub fn parse_search_reply(bytes: &[u8]) -> Result<Url, UpnpError> {
    let text = std::str::from_utf8(bytes).map_err(|_| UpnpError::Malformed("UTF-8 でない"))?;
    let mut lines = text.split("\r\n");
    let status = lines.next().unwrap_or("");
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(UpnpError::Malformed("状態が 200 でない"));
    }
    for l in lines {
        if let Some((k, v)) = l.split_once(':')
            && k.trim().eq_ignore_ascii_case("location")
        {
            return Url::parse(v);
        }
    }
    Err(UpnpError::Malformed("LOCATION が無い"))
}

/// `parse_search_reply` に、返事の送り手 `from` と LOCATION のアドレスが同じという条件を足したもの。違う返事は、別の機器へ
/// 誘導する形なので使わない。
pub fn parse_search_reply_from(bytes: &[u8], from: IpAddr) -> Result<Url, UpnpError> {
    let url = parse_search_reply(bytes)?;
    if from != IpAddr::V4(url.host) {
        return Err(UpnpError::Refused(
            "返事の送り手と LOCATION のアドレスが違う",
        ));
    }
    Ok(url)
}

// ---------------- HTTP ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// GET の本文。
pub fn get_request(url: &Url) -> Vec<u8> {
    format!(
        "GET {} HTTP/1.1\r\nHOST: {}\r\nConnection: close\r\n\r\n",
        url.path,
        url.host_header()
    )
    .into_bytes()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn decode_chunked(mut b: &[u8]) -> Result<Vec<u8>, UpnpError> {
    let bad = UpnpError::Malformed("chunked が壊れている");
    let mut out = Vec::new();
    loop {
        let eol = find(b, b"\r\n").ok_or_else(|| bad.clone())?;
        let size_text = std::str::from_utf8(&b[..eol]).map_err(|_| bad.clone())?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| bad.clone())?;
        b = &b[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size {
            return Err(bad);
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size..];
        b = b.strip_prefix(b"\r\n").ok_or_else(|| bad.clone())?;
        if out.len() > MAX_RESPONSE {
            return Err(UpnpError::Malformed("返事が大きすぎる"));
        }
    }
}

/// HTTP の返事を読む（接続を閉じるまで読んだ全部）。chunked と Content-Length に従う。本文が足りなければ断る。
pub fn parse_http_response(bytes: &[u8]) -> Result<HttpResponse, UpnpError> {
    if bytes.len() > MAX_RESPONSE + 4096 {
        return Err(UpnpError::Malformed("返事が大きすぎる"));
    }
    let split = find(bytes, b"\r\n\r\n").ok_or(UpnpError::Malformed("ヘッダの終わりが無い"))?;
    let head = std::str::from_utf8(&bytes[..split])
        .map_err(|_| UpnpError::Malformed("ヘッダが UTF-8 でない"))?;
    let rest = &bytes[split + 4..];
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    if !status_line.starts_with("HTTP/1.") {
        return Err(UpnpError::Malformed("HTTP の返事でない"));
    }
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or(UpnpError::Malformed("状態が読めない"))?;
    let mut length = None;
    let mut chunked = false;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        if k == "content-length" {
            length = v.parse::<usize>().ok();
        } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }
    let body: Vec<u8> = if chunked {
        decode_chunked(rest)?
    } else if let Some(n) = length {
        if rest.len() < n {
            return Err(UpnpError::Malformed("本文が Content-Length より短い"));
        }
        rest[..n].to_vec()
    } else {
        rest.to_vec()
    };
    if body.len() > MAX_RESPONSE {
        return Err(UpnpError::Malformed("返事が大きすぎる"));
    }
    Ok(HttpResponse {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

// ---------------- XML（必要な欄だけを読む） ----------------

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `<tag>…</tag>` の最初の 1 つの中身と、その後ろの残り。
fn element<'a>(s: &'a str, tag: &str) -> Option<(&'a str, &'a str)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let a = s.find(&open)? + open.len();
    let b = a + s[a..].find(&close)?;
    Some((s[a..b].trim(), &s[b + close.len()..]))
}

fn text_of(s: &str, tag: &str) -> Option<String> {
    element(s, tag).map(|(t, _)| decode_entities(t))
}

// ---------------- 機器の記述 ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// `urn:schemas-upnp-org:service:WANIPConnection:1` など（SOAP の名前空間と SOAPACTION に使う）。
    pub service_type: String,
    pub control: Url,
}

/// 機器の記述（XML）から WAN の接続のサービスを選ぶ。WANIPConnection を先に、無ければ WANPPPConnection。
/// 制御 URL は `<URLBase>`（あれば）か記述の URL `location` を基に解く。
pub fn find_wan_service(xml: &str, location: &Url) -> Result<Service, UpnpError> {
    let base = match text_of(xml, "URLBase") {
        Some(b) if !b.is_empty() => Url::parse(&b)?,
        _ => location.clone(),
    };
    if base.host != location.host {
        return Err(UpnpError::Refused("URLBase が機器と別のアドレス"));
    }
    let mut ip = None;
    let mut ppp = None;
    let mut rest = xml;
    while let Some((svc, after)) = element(rest, "service") {
        rest = after;
        let (Some(t), Some(c)) = (text_of(svc, "serviceType"), text_of(svc, "controlURL")) else {
            continue;
        };
        let slot = if t.starts_with("urn:schemas-upnp-org:service:WANIPConnection:") {
            &mut ip
        } else if t.starts_with("urn:schemas-upnp-org:service:WANPPPConnection:") {
            &mut ppp
        } else {
            continue;
        };
        if slot.is_none() {
            *slot = Some((t, c));
        }
    }
    let (service_type, control) = ip
        .or(ppp)
        .ok_or(UpnpError::Malformed("WAN の接続のサービスが無い"))?;
    Ok(Service {
        service_type,
        control: base.join(&control)?,
    })
}

// ---------------- SOAP ----------------

/// SOAP の POST 1 本（本文込み）。`args` は引数の名前と値（仕様の順で渡す）。
pub fn soap_request(service: &Service, action: &str, args: &[(&str, String)]) -> Vec<u8> {
    let mut inner = String::new();
    for (k, v) in args {
        inner.push_str(&format!("<{k}>{}</{k}>", escape(v)));
    }
    let body = format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{st}\">{inner}</u:{action}></s:Body></s:Envelope>\r\n",
        st = service.service_type
    );
    format!(
        "POST {path} HTTP/1.1\r\nHOST: {host}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {len}\r\nSOAPACTION: \"{st}#{action}\"\r\nConnection: close\r\n\r\n{body}",
        path = service.control.path,
        host = service.control.host_header(),
        len = body.len(),
        st = service.service_type
    )
    .into_bytes()
}

/// AddPortMapping の引数（UDP。外側と内側のポートは同じ値で頼む機器が多い）。`lease_secs` が 0 なら期限なし。
pub fn add_mapping_args(
    external: u16,
    internal: u16,
    client: Ipv4Addr,
    lease_secs: u32,
) -> Vec<(&'static str, String)> {
    vec![
        ("NewRemoteHost", String::new()),
        ("NewExternalPort", external.to_string()),
        ("NewProtocol", "UDP".to_string()),
        ("NewInternalPort", internal.to_string()),
        ("NewInternalClient", client.to_string()),
        ("NewEnabled", "1".to_string()),
        ("NewPortMappingDescription", MAPPING_DESCRIPTION.to_string()),
        ("NewLeaseDuration", lease_secs.to_string()),
    ]
}

/// GetSpecificPortMappingEntry / DeletePortMapping の引数。
pub fn mapping_key_args(external: u16) -> Vec<(&'static str, String)> {
    vec![
        ("NewRemoteHost", String::new()),
        ("NewExternalPort", external.to_string()),
        ("NewProtocol", "UDP".to_string()),
    ]
}

/// SOAP の返事の判定。状態 200 で errorCode が無ければ本文を返す。それ以外は `Fault`（errorCode が読めればその数）。
pub fn soap_result(r: &HttpResponse) -> Result<&str, UpnpError> {
    let code = text_of(&r.body, "errorCode").and_then(|c| c.trim().parse::<u32>().ok());
    if r.status == 200 && !r.body.contains("<errorCode>") {
        return Ok(&r.body);
    }
    Err(UpnpError::Fault {
        status: r.status,
        code,
    })
}

/// GetExternalIPAddress の答えから、ルーターの外側のアドレスを読む。
pub fn parse_external_ip(body: &str) -> Result<Ipv4Addr, UpnpError> {
    text_of(body, "NewExternalIPAddress")
        .and_then(|t| t.trim().parse().ok())
        .ok_or(UpnpError::Malformed("NewExternalIPAddress が読めない"))
}

/// ルーターの外側のアドレスと、STUN で見えた自分のアドレスの突き合わせの結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OuterCheck {
    /// 同じで、世界に通じるアドレス。割り当てたポートは外から届く。
    Match,
    /// STUN で見えた IPv4 のアドレスが無い（突き合わせる相手が無い）。
    NoStun,
    /// ルーターが外側のアドレスを答えなかった。
    Unknown,
    /// 違う、または世界に通じないアドレス。ルーターの外側にさらに NAT がある（二重 NAT・DS-Lite など）見込み。
    Behind,
}

/// UPnP で割り当てたポートが外から届くかの見立て。同じアドレスが STUN にも見えていて、それが世界に通じるアドレスのときだけ `Match`。
pub fn check_outer(stun: Option<SocketAddr>, router: Option<Ipv4Addr>) -> OuterCheck {
    let Some(SocketAddr::V4(stun)) = stun else {
        return OuterCheck::NoStun;
    };
    let Some(router) = router else {
        return OuterCheck::Unknown;
    };
    if router == *stun.ip() && guard::is_global_v4(router) {
        OuterCheck::Match
    } else {
        OuterCheck::Behind
    }
}

/// GetSpecificPortMappingEntry の答え。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub internal_port: u16,
    pub internal_client: String,
    pub enabled: bool,
    /// 割り当ての説明（無ければ空）。
    pub description: String,
    /// 残りの期限（秒。0 は期限なし）。
    pub lease_secs: u32,
}

pub fn parse_entry(body: &str) -> Result<Entry, UpnpError> {
    let internal_port = text_of(body, "NewInternalPort")
        .and_then(|t| t.trim().parse().ok())
        .ok_or(UpnpError::Malformed("NewInternalPort が読めない"))?;
    let internal_client = text_of(body, "NewInternalClient")
        .ok_or(UpnpError::Malformed("NewInternalClient が無い"))?;
    let enabled = text_of(body, "NewEnabled").is_none_or(|t| t.trim() != "0");
    let description = text_of(body, "NewPortMappingDescription").unwrap_or_default();
    let lease_secs = text_of(body, "NewLeaseDuration")
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0);
    Ok(Entry {
        internal_port,
        internal_client,
        enabled,
        description,
        lease_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOC: &str = "http://192.168.0.1:5000/rootDesc.xml";

    fn loc() -> Url {
        Url::parse(LOC).unwrap()
    }

    fn reply(lines: &[&str]) -> Vec<u8> {
        let mut s = lines.join("\r\n");
        s.push_str("\r\n\r\n");
        s.into_bytes()
    }

    #[test]
    fn the_search_request_names_the_multicast_group_and_target() {
        let t = String::from_utf8(search_request(SEARCH_TARGETS[0])).unwrap();
        assert!(t.starts_with("M-SEARCH * HTTP/1.1\r\n"));
        assert!(t.contains("HOST: 239.255.255.250:1900\r\n"));
        assert!(t.contains("MAN: \"ssdp:discover\"\r\n"));
        assert!(t.contains("ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n"));
        assert!(t.ends_with("\r\n\r\n"));
    }

    #[test]
    fn a_search_reply_gives_the_description_url() {
        let ok = reply(&[
            "HTTP/1.1 200 OK",
            "CACHE-CONTROL: max-age=120",
            "ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1",
            "USN: uuid:x::urn:schemas-upnp-org:device:InternetGatewayDevice:1",
            "Location: http://192.168.0.1:5000/rootDesc.xml",
            "SERVER: MiniUPnPd/1.6",
        ]);
        let u = parse_search_reply(&ok).unwrap();
        assert_eq!(
            (u.host, u.port, u.path.as_str()),
            (Ipv4Addr::new(192, 168, 0, 1), 5000, "/rootDesc.xml")
        );
    }

    #[test]
    fn broken_or_unsafe_search_replies_are_refused() {
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (
                reply(&["HTTP/1.1 500 Oops", &format!("LOCATION: {LOC}")]),
                "状態",
            ),
            (reply(&["HTTP/1.1 200 OK", "ST: x"]), "LOCATION が無い"),
            (
                reply(&["HTTP/1.1 200 OK", "LOCATION: https://192.168.0.1/d.xml"]),
                "https",
            ),
            (
                reply(&["HTTP/1.1 200 OK", "LOCATION: http://router.example/d.xml"]),
                "名前",
            ),
            (
                reply(&["HTTP/1.1 200 OK", "LOCATION: http://203.0.113.9:5000/d.xml"]),
                "外のアドレス",
            ),
            (
                reply(&["HTTP/1.1 200 OK", "LOCATION: http://192.168.0.1:0/d.xml"]),
                "ポート 0",
            ),
            (vec![0xFF, 0xFE, 0x00], "UTF-8"),
            (Vec::new(), "空"),
        ];
        for (bytes, why) in cases {
            assert!(parse_search_reply(&bytes).is_err(), "{why}");
        }
    }

    #[test]
    fn urls_accept_only_lan_http_ipv4() {
        for ok in [
            "http://10.0.0.1/a",
            "http://172.16.5.4:49152/ctl",
            "http://169.254.1.1:80",
            "http://127.0.0.1:8080/x?y=1",
            "HTTP://192.168.1.1/a",
        ] {
            assert!(Url::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://8.8.8.8/",
            "http://192.168.0.1.evil.test/",
            "http://[fe80::1]/",
            "ftp://192.168.0.1/",
            "http://192.168.0.1:99999/",
            "http://192.168.0.1:x/",
            "http://192.168.0.1/a b",
            "192.168.0.1/a",
            "http",
            "",
        ] {
            assert!(Url::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Url::parse("http://10.0.0.1").unwrap().path, "/");
        assert_eq!(Url::parse("http://10.0.0.1").unwrap().port, 80);
    }

    #[test]
    fn join_resolves_paths_and_refuses_another_host() {
        let base = loc();
        assert_eq!(base.join("/ctl/IPConn").unwrap().path, "/ctl/IPConn");
        assert_eq!(base.join("ctl").unwrap().path, "/ctl");
        assert_eq!(base.join("http://192.168.0.1:5000/x").unwrap().path, "/x");
        assert_eq!(
            base.join("http://192.168.0.1:5000/a?b=1&amp;c=2")
                .unwrap()
                .path,
            "/a?b=1&c=2"
        );
        assert!(base.join("http://192.168.0.2:5000/x").is_err());
        assert!(base.join("http://8.8.8.8/x").is_err());
    }

    #[test]
    fn join_refuses_control_characters_on_every_path() {
        let base = loc();
        for rel in [
            "/ctl\r\nHost: evil",
            "ctl\r\nHost: evil",
            "/ctl\nx",
            "ctl\tx",
            "/a b",
            "a b",
            "http://192.168.0.1:5000/x\r\nY: 1",
        ] {
            assert!(base.join(rel).is_err(), "{rel:?}");
        }
        // 前後の空白は取る（機器の記述の XML の改行・字下げ）。
        assert_eq!(base.join("\n  /ctl/IPConn\n").unwrap().path, "/ctl/IPConn");
    }

    #[test]
    fn a_search_reply_is_used_only_from_the_machine_it_names() {
        let ok = reply(&["HTTP/1.1 200 OK", "LOCATION: http://192.168.0.1:5000/d.xml"]);
        let ip = |s: &str| IpAddr::V4(s.parse().unwrap());
        assert_eq!(
            parse_search_reply_from(&ok, ip("192.168.0.1"))
                .unwrap()
                .host,
            Ipv4Addr::new(192, 168, 0, 1)
        );
        // 同じ LAN の別の機器が、ルーターを指す返事を送ってきた形
        assert!(parse_search_reply_from(&ok, ip("192.168.0.50")).is_err());
        assert!(parse_search_reply_from(&ok, IpAddr::V6("fe80::1".parse().unwrap())).is_err());
        // 送り手が合っても、中身が使えない返事は断る
        let bad = reply(&["HTTP/1.1 200 OK", "LOCATION: http://203.0.113.9/d.xml"]);
        assert!(parse_search_reply_from(&bad, ip("203.0.113.9")).is_err());
    }

    #[test]
    fn the_external_ip_is_read_and_a_broken_answer_is_refused() {
        let body = |ip: &str| {
            format!(
                "<u:GetExternalIPAddressResponse><NewExternalIPAddress>{ip}</NewExternalIPAddress></u:GetExternalIPAddressResponse>"
            )
        };
        assert_eq!(
            parse_external_ip(&body("198.51.100.20")).unwrap(),
            Ipv4Addr::new(198, 51, 100, 20)
        );
        for bad in ["", "not an ip", "1.2.3", "::1", "300.1.1.1"] {
            assert!(parse_external_ip(&body(bad)).is_err(), "{bad:?}");
        }
        assert!(parse_external_ip("<x/>").is_err());
    }

    #[test]
    fn the_outer_check_needs_the_same_global_address_on_both_sides() {
        let v4 = |s: &str| Some(s.parse::<SocketAddr>().unwrap());
        let ip = |s: &str| Some(s.parse::<Ipv4Addr>().unwrap());
        // 一致して世界に通じるアドレス（ポートは見ない）
        assert_eq!(
            check_outer(v4("8.8.4.4:50000"), ip("8.8.4.4")),
            OuterCheck::Match
        );
        // 違うアドレス: ルーターの外側にさらに NAT
        assert_eq!(
            check_outer(v4("8.8.4.4:50000"), ip("8.8.8.8")),
            OuterCheck::Behind
        );
        // 一致しても世界に通じないアドレス（私設・事業者の中・文書用）
        for p in ["192.168.1.2", "10.0.0.2", "100.64.1.2", "203.0.113.9"] {
            assert_eq!(
                check_outer(v4(&format!("{p}:5")), ip(p)),
                OuterCheck::Behind,
                "{p}"
            );
        }
        assert_eq!(check_outer(None, ip("8.8.4.4")), OuterCheck::NoStun);
        assert_eq!(
            check_outer(v4("[2001:db8::1]:5"), ip("8.8.4.4")),
            OuterCheck::NoStun
        );
        assert_eq!(check_outer(v4("8.8.4.4:5"), None), OuterCheck::Unknown);
    }

    fn http(status: &str, headers: &[&str], body: &str) -> Vec<u8> {
        let mut s = format!("HTTP/1.1 {status}\r\n");
        for h in headers {
            s.push_str(h);
            s.push_str("\r\n");
        }
        s.push_str("\r\n");
        s.push_str(body);
        s.into_bytes()
    }

    #[test]
    fn http_responses_follow_content_length_and_chunks() {
        let r = parse_http_response(&http("200 OK", &["Content-Length: 5"], "hello!!")).unwrap();
        assert_eq!((r.status, r.body.as_str()), (200, "hello"));
        let r = parse_http_response(&http("200 OK", &[], "until eof")).unwrap();
        assert_eq!(r.body, "until eof");
        let r = parse_http_response(&http(
            "200 OK",
            &["Transfer-Encoding: chunked"],
            "5\r\nhello\r\n3;x=1\r\nabc\r\n0\r\n\r\n",
        ))
        .unwrap();
        assert_eq!(r.body, "helloabc");
        let r = parse_http_response(&http("500 Internal Server Error", &[], "x")).unwrap();
        assert_eq!(r.status, 500);
    }

    #[test]
    fn broken_http_responses_are_refused() {
        for (bytes, why) in [
            (http("200 OK", &["Content-Length: 50"], "short"), "足りない"),
            (
                http("200 OK", &["Transfer-Encoding: chunked"], "zz\r\nab\r\n"),
                "chunked の長さ",
            ),
            (
                http("200 OK", &["Transfer-Encoding: chunked"], "5\r\nhe"),
                "chunked の途中",
            ),
            (b"SSH-2.0-OpenSSH\r\n\r\n".to_vec(), "HTTP でない"),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 0".to_vec(),
                "ヘッダの終わり",
            ),
            (b"HTTP/1.1 abc\r\n\r\n".to_vec(), "状態"),
            (vec![b'a'; MAX_RESPONSE + 5000], "大きすぎる"),
        ] {
            assert!(parse_http_response(&bytes).is_err(), "{why}");
        }
    }

    /// 実機（MiniUPnPd）の記述の形に合わせた合成の XML（入れ子の機器に WANIPConnection が 1 つ・WANPPPConnection が 1 つ）。
    fn description(base: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?>\n<root xmlns=\"urn:schemas-upnp-org:device-1-0\">{base}<device><deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType><serviceList><service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType><controlURL>/ctl/L3F</controlURL></service></serviceList><deviceList><device><deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType><deviceList><device><deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType><serviceList><service><serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType><controlURL>/ctl/PPP</controlURL></service><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:2</serviceType><controlURL>/ctl/IPConn</controlURL></service></serviceList></device></deviceList></device></deviceList></device></root>"
        )
    }

    #[test]
    fn the_wan_ip_service_is_preferred_over_ppp() {
        let s = find_wan_service(&description(""), &loc()).unwrap();
        assert_eq!(
            s.service_type,
            "urn:schemas-upnp-org:service:WANIPConnection:2"
        );
        assert_eq!(s.control.path, "/ctl/IPConn");
        assert_eq!(s.control.host, loc().host);
        assert_eq!(s.control.port, 5000);
    }

    #[test]
    fn a_ppp_only_device_and_a_url_base_are_read() {
        let xml = "<root><URLBase>http://192.168.0.1:49000/</URLBase><service><serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType><controlURL>upnp/control/wanppp</controlURL></service></root>";
        let s = find_wan_service(xml, &loc()).unwrap();
        assert!(s.service_type.contains("WANPPPConnection"));
        assert_eq!(
            (s.control.port, s.control.path.as_str()),
            (49000, "/upnp/control/wanppp")
        );
    }

    #[test]
    fn descriptions_without_a_wan_service_or_pointing_elsewhere_are_refused() {
        let none = "<root><service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType><controlURL>/c</controlURL></service></root>";
        assert!(find_wan_service(none, &loc()).is_err());
        assert!(find_wan_service("", &loc()).is_err());
        assert!(find_wan_service("<root><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>", &loc()).is_err());
        // URLBase / 制御 URL が機器と別のアドレス
        let other_base = description("<URLBase>http://192.168.0.9:5000/</URLBase>");
        assert!(find_wan_service(&other_base, &loc()).is_err());
        let abs = "<root><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>http://192.168.0.9:5000/c</controlURL></service></root>";
        assert!(find_wan_service(abs, &loc()).is_err());
        let outside = "<root><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>http://203.0.113.9/c</controlURL></service></root>";
        assert!(find_wan_service(outside, &loc()).is_err());
    }

    fn svc() -> Service {
        find_wan_service(&description(""), &loc()).unwrap()
    }

    #[test]
    fn the_add_mapping_request_carries_every_argument_in_order() {
        let args = add_mapping_args(47001, 47001, Ipv4Addr::new(192, 168, 0, 7), 3600);
        let req = String::from_utf8(soap_request(&svc(), "AddPortMapping", &args)).unwrap();
        assert!(req.starts_with("POST /ctl/IPConn HTTP/1.1\r\n"));
        assert!(req.contains("HOST: 192.168.0.1:5000\r\n"));
        assert!(req.contains(
            "SOAPACTION: \"urn:schemas-upnp-org:service:WANIPConnection:2#AddPortMapping\"\r\n"
        ));
        let (head, body) = req.split_once("\r\n\r\n").unwrap();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(len, body.len(), "Content-Length が本文と合う");
        let order = [
            "NewRemoteHost",
            "NewExternalPort",
            "NewProtocol",
            "NewInternalPort",
            "NewInternalClient",
            "NewEnabled",
            "NewPortMappingDescription",
            "NewLeaseDuration",
        ];
        let mut at = 0;
        for k in order {
            at += body[at..]
                .find(&format!("<{k}>"))
                .unwrap_or_else(|| panic!("{k}"));
        }
        assert!(body.contains("<NewRemoteHost></NewRemoteHost>"));
        assert!(body.contains("<NewExternalPort>47001</NewExternalPort>"));
        assert!(body.contains("<NewProtocol>UDP</NewProtocol>"));
        assert!(body.contains("<NewInternalClient>192.168.0.7</NewInternalClient>"));
        assert!(body.contains("<NewLeaseDuration>3600</NewLeaseDuration>"));
        assert!(body.contains(&format!(
            "<NewPortMappingDescription>{MAPPING_DESCRIPTION}</NewPortMappingDescription>"
        )));
        assert!(body.contains(
            "<u:AddPortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:2\">"
        ));
    }

    #[test]
    fn a_permanent_add_request_carries_lease_zero() {
        let args = add_mapping_args(47001, 47001, Ipv4Addr::new(192, 168, 0, 7), 0);
        let req = String::from_utf8(soap_request(&svc(), "AddPortMapping", &args)).unwrap();
        assert!(req.contains("<NewLeaseDuration>0</NewLeaseDuration>"));
        assert!(req.contains(&format!(
            "<NewPortMappingDescription>{MAPPING_DESCRIPTION}</NewPortMappingDescription>"
        )));
    }

    #[test]
    fn soap_values_are_escaped() {
        let req =
            String::from_utf8(soap_request(&svc(), "X", &[("A", "<&>".to_string())])).unwrap();
        assert!(req.contains("<A>&lt;&amp;&gt;</A>"));
    }

    #[test]
    fn soap_results_split_success_and_faults() {
        let ok = HttpResponse {
            status: 200,
            body: "<s:Envelope><s:Body><u:AddPortMappingResponse/></s:Body></s:Envelope>".into(),
        };
        assert!(soap_result(&ok).is_ok());
        let fault = |status, code: &str| HttpResponse {
            status,
            body: format!(
                "<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>{code}</errorCode><errorDescription>x</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"
            ),
        };
        assert_eq!(
            soap_result(&fault(500, "718")),
            Err(UpnpError::Fault {
                status: 500,
                code: Some(718)
            })
        );
        assert_eq!(
            soap_result(&fault(500, "725")).unwrap_err().fault_code(),
            Some(FAULT_ONLY_PERMANENT)
        );
        // 状態 200 でも errorCode が入っていれば断りとして扱う。
        assert!(soap_result(&fault(200, "501")).is_err());
        // 数でない errorCode・本文なし・HTML の断り
        for r in [
            fault(500, "abc"),
            HttpResponse {
                status: 500,
                body: String::new(),
            },
            HttpResponse {
                status: 404,
                body: "<html>Not Found</html>".into(),
            },
        ] {
            let e = soap_result(&r).unwrap_err();
            assert_eq!(e.fault_code(), None);
            assert!(matches!(e, UpnpError::Fault { code: None, .. }));
        }
    }

    #[test]
    fn a_mapping_entry_is_read_and_a_broken_one_is_refused() {
        let body = "<u:GetSpecificPortMappingEntryResponse><NewInternalPort>47001</NewInternalPort><NewInternalClient>192.168.0.7</NewInternalClient><NewEnabled>1</NewEnabled><NewPortMappingDescription>HiganRTool</NewPortMappingDescription><NewLeaseDuration>3590</NewLeaseDuration></u:GetSpecificPortMappingEntryResponse>";
        assert_eq!(
            parse_entry(body).unwrap(),
            Entry {
                internal_port: 47001,
                internal_client: "192.168.0.7".to_string(),
                enabled: true,
                description: MAPPING_DESCRIPTION.to_string(),
                lease_secs: 3590
            }
        );
        let bare = "<NewInternalPort>1</NewInternalPort><NewInternalClient>a</NewInternalClient>";
        assert_eq!(parse_entry(bare).unwrap().description, "");
        let off = body.replace("<NewEnabled>1<", "<NewEnabled>0<");
        assert!(!parse_entry(&off).unwrap().enabled);
        for broken in [
            "",
            "<NewInternalClient>192.168.0.7</NewInternalClient>",
            "<NewInternalPort>x</NewInternalPort><NewInternalClient>a</NewInternalClient>",
            "<NewInternalPort>70000</NewInternalPort><NewInternalClient>a</NewInternalClient>",
            "<NewInternalPort>1</NewInternalPort>",
        ] {
            assert!(parse_entry(broken).is_err(), "{broken}");
        }
    }

    #[test]
    fn fault_texts_name_the_code_but_not_any_address() {
        let t = UpnpError::Fault {
            status: 500,
            code: Some(718),
        }
        .text();
        assert!(t.contains("718"), "{t}");
        for e in [
            UpnpError::Refused("LAN の外のアドレス"),
            UpnpError::Malformed("x"),
        ] {
            assert!(!e.text().contains("192.168"));
        }
    }
}
