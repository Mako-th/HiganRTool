//! 子のプロセスへ渡す環境ブロック（`CreateProcessW` の lpEnvironment。pure）。
//!
//! ゲームごとの ini と台本の置き場を DLL へ伝える環境変数を、親の環境を変えずに子のためだけに組む。
//! 形は `名前=値\0` を並べて最後にもう 1 つ `\0`（UTF-16 なので `CREATE_UNICODE_ENVIRONMENT`）。並びは大文字小文字を区別しない順
//! （ASCII の英字だけ折る。ずれても子は起動する）。`=C:=C:\…` のような隠れた行もそのまま残す。

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;

fn fold(u: u16) -> u16 {
    if (u16::from(b'a')..=u16::from(b'z')).contains(&u) {
        u - 0x20
    } else {
        u
    }
}

fn cmp_name(a: &[u16], b: &[u16]) -> Ordering {
    a.iter().map(|&u| fold(u)).cmp(b.iter().map(|&u| fold(u)))
}

/// 上書きする名前として使えるか。空・`=` を含む・0 を含む名前は断る（`=` を入れると子が別の名前と読む）。
fn valid_override_name(name: &[u16]) -> bool {
    !name.is_empty() && !name.contains(&u16::from(b'=')) && !name.contains(&0)
}

/// 同名（大文字小文字を区別しない）は置き換え、無ければ足す。名前か値が使えなければ `None`。親の行に 0 があればその行だけ落とす。
pub fn build<I>(parent: I, overrides: &[(OsString, OsString)]) -> Option<Vec<u16>>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let wide = |s: &OsStr| s.encode_wide().collect::<Vec<u16>>();
    let mut over: Vec<(Vec<u16>, Vec<u16>)> = Vec::with_capacity(overrides.len());
    for (k, v) in overrides {
        let (k, v) = (wide(k), wide(v));
        if !valid_override_name(&k) || v.contains(&0) {
            return None;
        }
        if over.iter().any(|(o, _)| cmp_name(o, &k) == Ordering::Equal) {
            return None;
        }
        over.push((k, v));
    }
    let mut rows: Vec<(Vec<u16>, Vec<u16>)> = parent
        .into_iter()
        .map(|(k, v)| (wide(&k), wide(&v)))
        .filter(|(k, v)| !k.is_empty() && !k.contains(&0) && !v.contains(&0))
        .filter(|(k, _)| !over.iter().any(|(o, _)| cmp_name(o, k) == Ordering::Equal))
        .collect();
    rows.extend(over);
    // 安定した並べ替え（同名が親に 2 度あっても順を保つ）。
    rows.sort_by(|a, b| cmp_name(&a.0, &b.0));
    let mut block = Vec::new();
    for (k, v) in rows {
        block.extend_from_slice(&k);
        block.push(u16::from(b'='));
        block.extend_from_slice(&v);
        block.push(0);
    }
    // 空のブロックでも終端は 2 つの 0（1 つだと子が次のアドレスを読みに行く）。
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        v.iter().map(|(k, v)| (os(k), os(v))).collect()
    }

    fn rows_of(b: &[u16]) -> Vec<String> {
        assert!(b.len() >= 2, "{b:?}");
        assert_eq!(&b[b.len() - 2..], &[0, 0], "終端が 2 つの 0 でない");
        b[..b.len() - 1]
            .split(|&u| u == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf16(s).unwrap())
            .collect()
    }

    #[test]
    fn overrides_merge_into_the_parent_sorted() {
        let parent = pairs(&[
            ("Path", r"C:\Windows"),
            ("SystemRoot", r"C:\Windows"),
            ("=C:", r"C:\work"),
            ("higanrtool_ini", r"C:\old.ini"),
            ("zeta", "1"),
        ]);
        let b = build(
            parent,
            &pairs(&[
                ("HIGANRTOOL_INI", r"C:\a b\two_host.ini"),
                ("HIGANRTOOL_SCRIPT", r"C:\s\two_01_host.txt"),
            ]),
        )
        .unwrap();
        assert_eq!(
            rows_of(&b),
            vec![
                r"=C:=C:\work",
                r"HIGANRTOOL_INI=C:\a b\two_host.ini",
                r"HIGANRTOOL_SCRIPT=C:\s\two_01_host.txt",
                r"Path=C:\Windows",
                r"SystemRoot=C:\Windows",
                "zeta=1",
            ]
        );
    }

    #[test]
    fn refuses_bad_overrides() {
        for bad in [
            pairs(&[("A=B", "1")]),
            pairs(&[("", "1")]),
            vec![(os("A\0B"), os("1"))],
            vec![(os("A"), os("1\u{0}2"))],
            pairs(&[("HIGANRTOOL_INI", "a"), ("higanrtool_ini", "b")]),
        ] {
            assert!(build(Vec::new(), &bad).is_none(), "通した: {bad:?}");
        }
    }

    #[test]
    fn empty_parent_and_non_ascii_values() {
        assert_eq!(build(Vec::new(), &[]).unwrap(), vec![0, 0]);
        let b = build(Vec::new(), &pairs(&[("X", r"C:\ツール\a.ini")])).unwrap();
        assert_eq!(rows_of(&b), vec![r"X=C:\ツール\a.ini"]);
    }
}
