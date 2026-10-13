//! 観戦用の接続コードの候補のアドレスを書き換える（`peer_sim --rewrite-spectate-code`）。
//!
//! 本物の観戦者の dll（`code_side = spectate`）を、ホストの直の受信ソケットでなく `--shape` の中継越しに繋ぐための道具。
//! ホストの dll が書くコードの候補はホストの待ち受けアドレスそのもので（観戦用の別のアドレスを書く欄が無い）、観戦者は候補へ直に
//! JOIN するので、中継を通すには候補を中継のポートに書き換えたコードを渡すしかない。
//!
//! - 読み書きは core の `code::read_spectate` / `code::write_spectate`。検算はコピー間違い用で署名ではない（秘密は本文の中）ので、
//!   書き換えて付け直せば観戦者から見て正しいコードになる。session・秘密・名前・期限・種別は元のまま。
//! - 候補は置き換える（足さない）。観戦者は候補の全部へ JOIN を送りどれかから STREAM が来れば固定するので、
//!   元の受信ソケットを残すと中継を通らない経路ができ、揺らぎが掛からない。
//! - 候補にするのは中継の「観戦者向けの口」。`--a <ホストの受信ソケット> --b <観戦者の受信ソケット> --via-a X --via-b Y` なら
//!   観戦者（B）の宛先は `Y`。`X` だと送り元違いとして捨てられる（`shape::route`）。
//! - 書き換え先は `guard::check_peer` を通ったものだけ。期限切れ・対戦用・壊れたコードは断る（期限は延ばさない）。
#![forbid(unsafe_code)]

use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;

use higanr_core::candidate::{CandError, Candidates};
use higanr_core::code::{self, CodeError, Side};
use higanr_core::guard::{self, Scope};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewritten {
    pub text: String,
    /// 元の候補。
    pub from: Vec<SocketAddr>,
    pub to: SocketAddr,
    pub session_id: u32,
    pub expires: u64,
}

/// 元のコードを読めなかった理由を道具の文にする。対戦用のコードは種別で断る（渡すものの取り違え防止）。
fn read_error(e: CodeError) -> String {
    match e {
        CodeError::WrongSide(1 | 2) => {
            "対戦用のコード（種別 1・2）が渡された。観戦用のコード（ホストの spectate_code.txt の中身。種別 3）を渡す"
                .to_string()
        }
        CodeError::Expired { expires, now } => {
            format!("元のコードは期限切れ（期限 {expires}・今 {now}。UNIX 秒）。新しい観戦用のコードで書き換える")
        }
        // コードのバージョンはパケットのバージョンと同じ数（`wire::PROTO_VER`）。
        CodeError::BadVersion(theirs) => {
            higanr_core::net_menu::other_version_text(higanr_core::wire::PROTO_VER, theirs)
        }
        CodeError::OtherBuild { theirs } => {
            higanr_core::net_menu::other_build_text(higanr_core::update::BUILD_NUMBER, theirs)
        }
        e => format!("観戦用のコードとして読めない: {e:?}（コピー間違い・途中で切れた・壊れたコード）"),
    }
}

fn cand_error(e: CandError) -> String {
    format!("候補にできないアドレス: {e:?}")
}

pub fn rewrite(
    text: &str,
    candidate: SocketAddr,
    scope: Scope,
    now: u64,
) -> Result<Rewritten, String> {
    guard::check_peer(candidate, scope).map_err(|e| format!("--candidate: {e}"))?;
    let s = code::read_spectate(text, now).map_err(read_error)?;
    let c = s.code;
    let from: Vec<SocketAddr> = c.candidates.as_slice().iter().map(|x| x.addr).collect();
    let new = code::Code {
        candidates: Candidates::from_addrs(&[candidate]).map_err(cand_error)?,
        ..c
    };
    // 書き手が種別を確かめるので、対戦用に化けることは無い。
    debug_assert_eq!(new.side, Side::Spectate);
    // 公開鍵も読んだ値のまま。
    let out = code::write_spectate(&new, &s.host_public)
        .map_err(|e| format!("観戦用のコードを書けない: {e:?}"))?;
    Ok(Rewritten {
        text: out,
        from,
        to: candidate,
        session_id: c.session_id,
        expires: c.expires,
    })
}

/// ファイルから読んで書き換え、`output` へ書く。出力先がゲームのフォルダの中なら断る。既にあれば `overwrite` が無いと断る。
/// 末尾は dll の `spectate_code.txt` と同じ CRLF。
pub fn rewrite_files(
    input: &Path,
    output: &Path,
    candidate: SocketAddr,
    scope: Scope,
    overwrite: bool,
    now: u64,
) -> Result<Rewritten, String> {
    let raw = std::fs::read(input).map_err(|e| format!("{} が読めない: {e}", input.display()))?;
    let text = String::from_utf8(raw)
        .map_err(|_| format!("{} が UTF-8 の文字列でない", input.display()))?;
    let r = rewrite(text.trim_start_matches('\u{FEFF}'), candidate, scope, now)?;
    let dir = match output.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    crate::record::refuse_game_folder(dir)?;
    let line = format!("{}\r\n", r.text);
    let mut open = std::fs::OpenOptions::new();
    open.write(true);
    if overwrite {
        open.create(true).truncate(true);
    } else {
        open.create_new(true);
    }
    let mut f = open.open(output).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!(
                "{} が既にある（前の回の古いコードかもしれない。消すか、置き換えるなら --overwrite）",
                output.display()
            )
        } else {
            format!("{} を開けない: {e}", output.display())
        }
    })?;
    f.write_all(line.as_bytes())
        .map_err(|e| format!("{} に書けない: {e}", output.display()))?;
    Ok(r)
}

pub fn summary(r: &Rewritten) -> String {
    let from: Vec<String> = r.from.iter().map(ToString::to_string).collect();
    format!(
        "peer_sim: 観戦用のコードの候補を書き換えた: {} → {}（session 0x{:08X}・期限 UNIX 秒 {}。種別・秘密・名前は元のまま）",
        from.join(" "),
        r.to,
        r.session_id,
        r.expires
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::auth::Secret;
    use higanr_core::handshake::Name;

    const NOW: u64 = 1_000_000;
    const SECRET: Secret = Secret(*b"rewrite-test-key");
    const HOST_PUBLIC: [u8; 32] = [
        0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x3E,
        0x3F, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x4B, 0x4C, 0x4D,
        0x4E, 0x4F,
    ];

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn original(side: Side, cands: &[&str], expires: u64) -> String {
        let addrs: Vec<SocketAddr> = cands.iter().map(|s| a(s)).collect();
        let c = code::Code {
            side,
            session_id: 0x1357_9BDF,
            secret: (side != Side::Responder).then_some(SECRET),
            name: Name::new("host-x"),
            nat: higanr_core::nat::NatMark::UNKNOWN,
            candidates: Candidates::from_addrs(&addrs).unwrap(),
            expires,
        };
        match side {
            Side::Spectate => code::write_spectate(&c, &HOST_PUBLIC).unwrap(),
            Side::Initiator => code::write_initiator(&c).unwrap(),
            Side::Responder => code::write_reply(&c, &SECRET).unwrap(),
            Side::SpectateReply => code::write_spectate_reply(&c, &SECRET).unwrap(),
        }
    }

    #[test]
    fn rewritten_code_reads_back_with_the_new_candidate_and_the_same_rest() {
        let src = original(
            Side::Spectate,
            &["127.0.0.1:47001", "127.0.0.2:47001"],
            NOW + 500,
        );
        let r = rewrite(&src, a("127.0.0.1:47022"), Scope::Loopback, NOW).unwrap();
        assert_ne!(r.text, src);
        let got_full =
            code::read_spectate(&r.text, NOW).expect("観戦者の側の読みで読める（検算が合う）");
        assert_eq!(got_full.host_public, HOST_PUBLIC);
        let got = got_full.code;
        let want = code::read_spectate(&src, NOW).unwrap().code;
        let addrs: Vec<_> = got.candidates.as_slice().iter().map(|x| x.addr).collect();
        assert_eq!(addrs, vec![a("127.0.0.1:47022")], "候補は新しい 1 つだけ");
        assert_eq!(got.side, Side::Spectate);
        assert_eq!(got.session_id, want.session_id);
        assert_eq!(got.secret, want.secret);
        assert_eq!(got.name, want.name);
        assert_eq!(got.expires, want.expires);
        assert_eq!((got.session_id, got.expires), (0x1357_9BDF, NOW + 500));
        assert_eq!(r.from, vec![a("127.0.0.1:47001"), a("127.0.0.2:47001")]);
        assert_eq!(
            (r.to, r.session_id, r.expires),
            (a("127.0.0.1:47022"), 0x1357_9BDF, NOW + 500)
        );
    }

    /// 検算を付け直さずに本文だけ替えた偽物（末尾 4 B を元のコードのものに戻した形）が `BadCheck` で断られる。
    #[test]
    fn the_check_bytes_depend_on_the_candidate() {
        let src = original(Side::Spectate, &["127.0.0.1:47001"], NOW + 500);
        let r = rewrite(&src, a("127.0.0.1:47022"), Scope::Loopback, NOW).unwrap();
        let mut bytes = code::b64_decode(&r.text).unwrap();
        let orig = code::b64_decode(&src).unwrap();
        assert_eq!(bytes.len(), orig.len(), "同じ形の候補 1 つ（長さが同じ）");
        let n = bytes.len();
        bytes[n - code::CHECK_LEN..].copy_from_slice(&orig[n - code::CHECK_LEN..]);
        let forged = code::b64_encode(&bytes);
        assert_eq!(
            code::read_spectate(&forged, NOW),
            Err(CodeError::BadCheck),
            "元の検算のままでは読めない（検算が本文を見ている）"
        );
        assert!(code::read_spectate(&r.text, NOW).is_ok());
    }

    #[test]
    fn refuses_wrong_kind_broken_expired_and_unreachable_targets() {
        let to = a("127.0.0.1:47022");
        let ok = original(Side::Spectate, &["127.0.0.1:47001"], NOW + 500);
        for side in [Side::Initiator, Side::Responder] {
            let text = original(side, &["127.0.0.1:47001"], NOW + 500);
            let e = rewrite(&text, to, Scope::Loopback, NOW).unwrap_err();
            assert!(e.contains("対戦用のコード"), "{side:?}: {e}");
        }
        let cut = &ok[..ok.len() - 3];
        assert!(rewrite(cut, to, Scope::Loopback, NOW).is_err());
        let mut bad = ok.clone().into_bytes();
        let i = bad.len() / 2;
        bad[i] = if bad[i] == b'A' { b'B' } else { b'A' };
        let bad = String::from_utf8(bad).unwrap();
        assert!(rewrite(&bad, to, Scope::Loopback, NOW).is_err());
        assert!(rewrite("", to, Scope::Loopback, NOW).is_err());
        // 期限は延ばさない。期限ちょうどの秒までは使える。
        let old = original(Side::Spectate, &["127.0.0.1:47001"], NOW - 1);
        assert!(
            rewrite(&old, to, Scope::Loopback, NOW)
                .unwrap_err()
                .contains("期限切れ")
        );
        let edge = original(Side::Spectate, &["127.0.0.1:47001"], NOW);
        assert!(rewrite(&edge, to, Scope::Loopback, NOW).is_ok());
        for bad in [
            "192.168.1.9:47022",
            "8.8.8.8:47022",
            "127.0.0.1:0",
            "0.0.0.0:47022",
        ] {
            assert!(
                rewrite(&ok, a(bad), Scope::Loopback, NOW).is_err(),
                "{bad} を通した"
            );
        }
        assert!(rewrite(&ok, a("192.168.1.9:47022"), Scope::Lan, NOW).is_ok());
        assert!(rewrite(&ok, a("8.8.8.8:47022"), Scope::Lan, NOW).is_err());
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "higanrtool_spectate_code_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn files_are_written_new_only_and_the_input_is_kept() {
        let d = temp_dir("files");
        let src = original(Side::Spectate, &["127.0.0.1:47001"], NOW + 500);
        let input = d.join("spectate_code.txt");
        std::fs::write(&input, format!("{src}\r\n")).unwrap();
        let out = d.join("peer_code.txt");
        let to = a("127.0.0.1:47022");
        let r = rewrite_files(&input, &out, to, Scope::Loopback, false, NOW).unwrap();
        let written = std::fs::read_to_string(&out).unwrap();
        assert_eq!(written, format!("{}\r\n", r.text));
        let back = code::read_spectate(&written, NOW).unwrap().code;
        assert_eq!(back.candidates.as_slice()[0].addr, to);
        assert_eq!(
            std::fs::read_to_string(&input).unwrap(),
            format!("{src}\r\n")
        );
        let other = a("127.0.0.1:47023");
        let e = rewrite_files(&input, &out, other, Scope::Loopback, false, NOW).unwrap_err();
        assert!(e.contains("既にある"), "{e}");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), written);
        rewrite_files(&input, &out, other, Scope::Loopback, true, NOW).unwrap();
        let again = code::read_spectate(&std::fs::read_to_string(&out).unwrap(), NOW)
            .unwrap()
            .code;
        assert_eq!(again.candidates.as_slice()[0].addr, other);
        let bom = d.join("bom.txt");
        std::fs::write(&bom, format!("\u{FEFF}{src}\n")).unwrap();
        rewrite_files(&bom, &d.join("out2.txt"), to, Scope::Loopback, false, NOW).unwrap();
        let broken = d.join("broken.txt");
        std::fs::write(&broken, "not-a-code").unwrap();
        let out3 = d.join("out3.txt");
        assert!(rewrite_files(&broken, &out3, to, Scope::Loopback, false, NOW).is_err());
        let far = a("8.8.8.8:1");
        assert!(rewrite_files(&input, &out3, far, Scope::Loopback, false, NOW).is_err());
        let none = d.join("none.txt");
        assert!(rewrite_files(&none, &out3, to, Scope::Loopback, false, NOW).is_err());
        assert!(!out3.exists(), "断ったのに出力を作った");
        // 合成の空の th09.exe でゲームのフォルダに見立てる。
        let g = d.join("game");
        std::fs::create_dir_all(&g).unwrap();
        std::fs::write(g.join("th09.exe"), b"").unwrap();
        let into = g.join("peer_code.txt");
        let e = rewrite_files(&input, &into, to, Scope::Loopback, true, NOW).unwrap_err();
        assert!(e.contains("ゲームのフォルダ"), "{e}");
        assert!(!into.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn summary_names_the_candidates_but_not_the_secret() {
        let src = original(Side::Spectate, &["127.0.0.1:47001"], NOW + 500);
        let r = rewrite(&src, a("127.0.0.1:47022"), Scope::Loopback, NOW).unwrap();
        let s = summary(&r);
        assert!(
            s.contains("127.0.0.1:47001") && s.contains("127.0.0.1:47022"),
            "{s}"
        );
        assert!(s.contains("0x13579BDF"), "{s}");
        assert!(!s.contains("rewrite-test-key"), "{s}");
        assert!(!s.contains(&r.text), "コードそのものを出さない");
    }
}
