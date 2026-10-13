//! 開発の形で、`build\bin\` の成果物がソースより古ければ起動を断る（建て直さずに古い DLL で実機を起こす取り違えを止める）。
//!
//! - 開発の形 = この exe が `<R>\build\bin\` にあり、`<R>\Cargo.toml` と `<R>\src\` があるとき。配る形では何も調べない。
//! - 比べるソースは、cargo が成果物の隣に書く dep-info（`<出力>.d`）から取る。`src\` を歩くと、`#[cfg(test)]` の中だけの
//!   ファイルを変えても cargo は DLL を作り直さず、build.bat を回しても直らない断りになる。Cargo.toml / Cargo.lock も同じ理由で見ない。
//! - 相手は `build\bin\` の成果物の更新時刻（build.bat の `copy` は時刻を保つ）。
//! - dep-info が無い・読めない・崩れているときは**断る**。通すと「ソース 0 本 ⇒ 新しいものは無い」の合格になり、ガードが黙って効かなくなる。
//! - 抜け道の引数は作らない。古い成果物で起こしたいときは `build\bin\` の外へコピーする。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `<R>` から release の出力まで（`.cargo\config.toml` の target と build.bat の `RELDIR` に合わせる）。
const RELEASE_DIR: [&str; 3] = ["target", "i686-pc-windows-msvc", "release"];
/// DLL の dep-info（`[lib] name = "higanrtool"` による）。
const DLL_DEP_INFO: &str = "higanrtool.d";
/// ローダーの dep-info（`[[bin]] name = "higanrtool_loader"` による）。
const LOADER_DEP_INFO: &str = "higanrtool_loader.d";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    NotDev,
    Fresh,
    Stale(Vec<Stale>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stale {
    /// dep-info が無い・読めない・形が崩れている。
    DepInfo { path: PathBuf, why: String },
    /// 比べる相手の成果物の時刻が読めない。
    Artifact { path: PathBuf, why: String },
    /// dep-info に載っているソースが成果物より新しい。
    Source {
        artifact: PathBuf,
        artifact_time: SystemTime,
        violation: Violation,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Violation {
    /// cargo も作り直すので、新しいのと同じに扱う。
    Missing(PathBuf),
    Newer(PathBuf, SystemTime),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DepInfoError {
    Empty,
    NoSeparator(usize),
    NoTarget(usize),
}

/// `<R>\build\bin\<exe>` の形でなければ `None`。大文字小文字は区別しない。
fn root_of(exe: &Path) -> Option<&Path> {
    let bin = exe.parent()?;
    let build = bin.parent()?;
    let named = |p: &Path, n: &str| {
        p.file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case(n))
    };
    if named(bin, "bin") && named(build, "build") {
        build.parent().filter(|r| !r.as_os_str().is_empty())
    } else {
        None
    }
}

/// dep-info（Makefile の規則の形）を読み、全部の規則の入力を返す。続き行・複数の規則・`#` 行・CRLF を読む。
/// パスの空白は `\ `、ドライブ文字の `C:\` は区切りにならない。
pub(crate) fn parse_dep_info(text: &str) -> Result<Vec<PathBuf>, DepInfoError> {
    let mut deps = Vec::new();
    let mut rules = 0usize;
    for (i, line) in logical_lines(text).iter().enumerate() {
        let n = i + 1;
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let words = split_words(t);
        let Some(sep) = words.iter().position(|w| w.ends_with(':')) else {
            return Err(DepInfoError::NoSeparator(n));
        };
        let target_is_empty = words[..sep].is_empty() && words[sep].len() == 1;
        if target_is_empty {
            return Err(DepInfoError::NoTarget(n));
        }
        rules += 1;
        deps.extend(words[sep + 1..].iter().map(PathBuf::from));
    }
    if rules == 0 || deps.is_empty() {
        return Err(DepInfoError::Empty);
    }
    Ok(deps)
}

fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        match line.strip_suffix('\\') {
            Some(head) => {
                cur.push_str(head);
                cur.push(' ');
            }
            None => {
                cur.push_str(line);
                out.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&' ') {
            cur.push(' ');
            chars.next();
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// 建て直しが要る理由を 1 つ返す。時刻が読めないソースを先に、次に成果物より真に新しいもののうちいちばん新しいもの（同じ時刻は通す）。
pub(crate) fn newest_violation(
    artifact: SystemTime,
    sources: &[(PathBuf, Option<SystemTime>)],
) -> Option<Violation> {
    if let Some((p, _)) = sources.iter().find(|(_, t)| t.is_none()) {
        return Some(Violation::Missing(p.clone()));
    }
    sources
        .iter()
        .filter_map(|(p, t)| t.map(|t| (p, t)))
        .filter(|(_, t)| *t > artifact)
        .max_by_key(|(_, t)| *t)
        .map(|(p, t)| Violation::Newer(p.clone(), t))
}

pub(crate) fn check(exe: &Path, dll: &Path) -> Verdict {
    let Some(root) = root_of(exe) else {
        return Verdict::NotDev;
    };
    if !root.join("Cargo.toml").is_file() || !root.join("src").is_dir() {
        return Verdict::NotDev;
    }
    check_at(root, exe, dll)
}

fn check_at(root: &Path, exe: &Path, dll: &Path) -> Verdict {
    let release = RELEASE_DIR
        .iter()
        .fold(root.to_path_buf(), |p, s| p.join(s));
    let mut stale = Vec::new();
    for (artifact, dep_info) in [(dll, DLL_DEP_INFO), (exe, LOADER_DEP_INFO)] {
        if let Err(s) = check_one(root, artifact, &release.join(dep_info)) {
            stale.push(s);
        }
    }
    if stale.is_empty() {
        Verdict::Fresh
    } else {
        Verdict::Stale(stale)
    }
}

fn check_one(root: &Path, artifact: &Path, dep_info: &Path) -> Result<(), Stale> {
    let text = std::fs::read_to_string(dep_info).map_err(|e| Stale::DepInfo {
        path: dep_info.to_path_buf(),
        why: e.to_string(),
    })?;
    let deps = parse_dep_info(&text).map_err(|e| Stale::DepInfo {
        path: dep_info.to_path_buf(),
        why: format!("{e:?}"),
    })?;
    let artifact_time = modified(artifact).map_err(|e| Stale::Artifact {
        path: artifact.to_path_buf(),
        why: e.to_string(),
    })?;
    // 相対パスは `<R>` 基準に直す（`build.dep-info-basedir` を置いたときの形）。
    let sources: Vec<(PathBuf, Option<SystemTime>)> = deps
        .into_iter()
        .map(|p| if p.is_absolute() { p } else { root.join(p) })
        .map(|p| {
            let t = modified(&p).ok();
            (p, t)
        })
        .collect();
    match newest_violation(artifact_time, &sources) {
        None => Ok(()),
        Some(violation) => Err(Stale::Source {
            artifact: artifact.to_path_buf(),
            artifact_time,
            violation,
        }),
    }
}

fn modified(p: &Path) -> std::io::Result<SystemTime> {
    std::fs::metadata(p)?.modified()
}

pub(crate) fn describe(s: &Stale) -> String {
    match s {
        Stale::DepInfo { path, why } => {
            format!(
                "cargo の依存の一覧 {} が読めません（{why}）",
                path.display()
            )
        }
        Stale::Artifact { path, why } => format!("{} の時刻が読めません（{why}）", path.display()),
        Stale::Source {
            artifact,
            artifact_time,
            violation: Violation::Missing(src),
        } => format!(
            "{} の元のソース {} が無いか読めません（成果物 {}）",
            artifact.display(),
            src.display(),
            fmt_utc(*artifact_time)
        ),
        Stale::Source {
            artifact,
            artifact_time,
            violation: Violation::Newer(src, t),
        } => format!(
            "ソース {}（{}）が {}（{}）より {} 秒新しい",
            src.display(),
            fmt_utc(*t),
            artifact.display(),
            fmt_utc(*artifact_time),
            t.duration_since(*artifact_time)
                .unwrap_or(Duration::ZERO)
                .as_secs()
        ),
    }
}

/// UTC のまま出す（時差を引くには Win32 が要り、比べるのは同じ時計なので読み違えない）。
fn fmt_utc(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_790_000_000 + secs)
    }

    #[test]
    fn parses_the_cargo_one_line_form_with_drive_letters() {
        let d = r"C:\R\target\i686-pc-windows-msvc\release\higanrtool.dll: C:\R\src\core\src\lib.rs C:\R\src\dll\src\lib.rs D:\x\y.rs";
        assert_eq!(
            parse_dep_info(d),
            Ok(vec![
                PathBuf::from(r"C:\R\src\core\src\lib.rs"),
                PathBuf::from(r"C:\R\src\dll\src\lib.rs"),
                PathBuf::from(r"D:\x\y.rs"),
            ])
        );
    }

    #[test]
    fn escaped_spaces_stay_inside_one_path() {
        let d =
            "C:\\My\\ Tools\\R\\x.dll: C:\\My\\ Tools\\R\\src\\a\\ b.rs C:\\sp\\\\ lead\\c.rs\n";
        assert_eq!(
            parse_dep_info(d),
            Ok(vec![
                PathBuf::from(r"C:\My Tools\R\src\a b.rs"),
                PathBuf::from(r"C:\sp\ lead\c.rs"),
            ])
        );
    }

    #[test]
    fn continuation_lines_rules_comments_and_crlf() {
        let d = "C:\\o.dll: C:\\a.rs \\\r\n  C:\\b.rs\r\n# env-dep:CARGO_PKG_NAME=x\r\n\r\nC:\\o.dll.lib: C:\\c.rs\r\nC:\\a.rs:\r\n";
        assert_eq!(
            parse_dep_info(d),
            Ok(vec![
                PathBuf::from(r"C:\a.rs"),
                PathBuf::from(r"C:\b.rs"),
                PathBuf::from(r"C:\c.rs"),
            ])
        );
    }

    #[test]
    fn poison_forms_are_errors_not_empty_lists() {
        assert_eq!(parse_dep_info(""), Err(DepInfoError::Empty));
        assert_eq!(parse_dep_info(" \r\n\n"), Err(DepInfoError::Empty));
        assert_eq!(parse_dep_info("# only\n"), Err(DepInfoError::Empty));
        assert_eq!(parse_dep_info(r"C:\o.dll:"), Err(DepInfoError::Empty));
        assert_eq!(
            parse_dep_info(r"C:\o.dll C:\a.rs"),
            Err(DepInfoError::NoSeparator(1))
        );
        assert_eq!(
            parse_dep_info(r"C:\o.dll:C:\a.rs"),
            Err(DepInfoError::NoSeparator(1))
        );
        assert_eq!(
            parse_dep_info("C:\\o.dll: C:\\a.rs\n: C:\\b.rs\n"),
            Err(DepInfoError::NoTarget(2))
        );
        assert_eq!(
            parse_dep_info("C:\\o.dll: C:\\a.rs \\"),
            Ok(vec![PathBuf::from(r"C:\a.rs")])
        );
    }

    #[test]
    fn newest_violation_is_strictly_newer_and_missing_first() {
        let a = |s: &str, x: Option<u64>| (PathBuf::from(s), x.map(t));
        assert_eq!(newest_violation(t(100), &[]), None);
        assert_eq!(
            newest_violation(t(100), &[a("old", Some(10)), a("same", Some(100))]),
            None
        );
        assert_eq!(
            newest_violation(
                t(100),
                &[a("n1", Some(101)), a("n3", Some(300)), a("n2", Some(200))]
            ),
            Some(Violation::Newer("n3".into(), t(300)))
        );
        assert_eq!(
            newest_violation(t(100), &[a("n", Some(300)), a("gone", None)]),
            Some(Violation::Missing("gone".into()))
        );
    }

    #[test]
    fn utc_format_matches_known_dates() {
        let at = |s: u64| fmt_utc(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(at(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(at(1_704_067_199), "2023-12-31 23:59:59 UTC");
    }

    #[test]
    fn root_only_from_build_bin() {
        assert_eq!(
            root_of(Path::new(r"C:\R\build\bin\HiganRTool.exe")),
            Some(Path::new(r"C:\R"))
        );
        assert_eq!(
            root_of(Path::new(r"C:\R\Build\BIN\HiganRTool.exe")),
            Some(Path::new(r"C:\R"))
        );
        assert_eq!(
            root_of(Path::new(
                r"C:\R\target\i686-pc-windows-msvc\release\higanrtool_loader.exe"
            )),
            None
        );
        assert_eq!(root_of(Path::new(r"C:\R\bin\HiganRTool.exe")), None);
        assert_eq!(
            root_of(Path::new(r"C:\Games\HiganRTool\HiganRTool.exe")),
            None
        );
        assert_eq!(root_of(Path::new(r"build\bin\HiganRTool.exe")), None);
    }

    struct Tree {
        top: PathBuf,
        root: PathBuf,
    }

    impl Tree {
        fn new(tag: &str) -> Tree {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let top = std::env::temp_dir().join(format!(
                "HiganRTool_freshness_{tag}_{}_{nanos}",
                std::process::id()
            ));
            let root = top.join("R dir");
            for d in [
                "src/core/src",
                "src/dll/src/netplay",
                "src/loader/src",
                "src/peer_sim/src",
                "build/bin",
                "target/i686-pc-windows-msvc/release",
            ] {
                std::fs::create_dir_all(root.join(d)).unwrap();
            }
            let tree = Tree { top, root };
            tree.file("Cargo.toml", 0);
            for f in [
                "src/core/src/lib.rs",
                "src/dll/src/lib.rs",
                "src/dll/src/netplay/tests.rs",
                "src/loader/src/main.rs",
                "src/peer_sim/src/main.rs",
            ] {
                tree.file(f, 10);
            }
            tree.file("build/bin/HiganRTool.dll", 100);
            tree.file("build/bin/HiganRTool.exe", 100);
            // dep-info: DLL は core と dll の lib.rs、ローダーは core と loader（peer_sim はどちらにも載らない）。
            tree.dep_info(
                DLL_DEP_INFO,
                "higanrtool.dll",
                &["src/core/src/lib.rs", "src/dll/src/lib.rs"],
            );
            tree.dep_info(
                LOADER_DEP_INFO,
                "higanrtool_loader.exe",
                &["src/core/src/lib.rs", "src/loader/src/main.rs"],
            );
            tree
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        fn file(&self, rel: &str, secs: u64) {
            std::fs::write(self.path(rel), b"x").unwrap();
            self.touch(rel, secs);
        }

        fn touch(&self, rel: &str, secs: u64) {
            File::options()
                .write(true)
                .open(self.path(rel))
                .unwrap()
                .set_modified(t(secs))
                .unwrap();
        }

        fn dep_info(&self, name: &str, out: &str, deps: &[&str]) {
            let esc = |p: PathBuf| p.display().to_string().replace(' ', "\\ ");
            let release = self.path("target/i686-pc-windows-msvc/release");
            let mut s = format!("{}:", esc(release.join(out)));
            for d in deps {
                s.push(' ');
                s.push_str(&esc(self.path(d)));
            }
            s.push('\n');
            std::fs::write(release.join(name), s).unwrap();
        }

        fn verdict(&self) -> Verdict {
            check(
                &self.path("build/bin/HiganRTool.exe"),
                &self.path("build/bin/HiganRTool.dll"),
            )
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.top);
        }
    }

    fn newer_paths(v: &Verdict) -> Vec<(PathBuf, PathBuf)> {
        match v {
            Verdict::Stale(list) => list
                .iter()
                .map(|s| match s {
                    Stale::Source {
                        artifact,
                        violation: Violation::Newer(src, _),
                        ..
                    } => (artifact.clone(), src.clone()),
                    other => panic!("新しいソース以外の理由: {other:?}"),
                })
                .collect(),
            other => panic!("断っていない: {other:?}"),
        }
    }

    #[test]
    fn tree_with_old_or_equal_sources_passes() {
        let tree = Tree::new("pass");
        assert_eq!(tree.verdict(), Verdict::Fresh);
        tree.touch("src/dll/src/lib.rs", 100);
        tree.touch("src/core/src/lib.rs", 100);
        assert_eq!(tree.verdict(), Verdict::Fresh);
    }

    #[test]
    fn newer_dll_source_is_refused() {
        let tree = Tree::new("dll");
        tree.touch("src/dll/src/lib.rs", 200);
        assert_eq!(
            newer_paths(&tree.verdict()),
            vec![(
                tree.path("build/bin/HiganRTool.dll"),
                tree.path("src/dll/src/lib.rs")
            )]
        );
    }

    #[test]
    fn newer_core_refuses_both_and_newer_loader_refuses_the_exe() {
        let tree = Tree::new("core");
        tree.touch("src/core/src/lib.rs", 200);
        assert_eq!(
            newer_paths(&tree.verdict()),
            vec![
                (
                    tree.path("build/bin/HiganRTool.dll"),
                    tree.path("src/core/src/lib.rs")
                ),
                (
                    tree.path("build/bin/HiganRTool.exe"),
                    tree.path("src/core/src/lib.rs")
                ),
            ]
        );
        tree.touch("src/core/src/lib.rs", 10);
        tree.touch("src/loader/src/main.rs", 200);
        tree.touch("build/bin/HiganRTool.exe", 150);
        assert_eq!(
            newer_paths(&tree.verdict()),
            vec![(
                tree.path("build/bin/HiganRTool.exe"),
                tree.path("src/loader/src/main.rs")
            )]
        );
    }

    #[test]
    fn unlisted_sources_do_not_refuse() {
        let tree = Tree::new("unlisted");
        tree.touch("src/peer_sim/src/main.rs", 500);
        tree.touch("src/dll/src/netplay/tests.rs", 500);
        tree.touch("Cargo.toml", 500);
        assert_eq!(tree.verdict(), Verdict::Fresh);
        tree.touch("src/dll/src/lib.rs", 101);
        assert_eq!(newer_paths(&tree.verdict()).len(), 1);
    }

    #[test]
    fn rebuild_clears_the_refusal_and_only_the_rebuild_does() {
        let tree = Tree::new("rebuild");
        tree.touch("src/dll/src/lib.rs", 200);
        tree.touch("src/dll/src/netplay/tests.rs", 200);
        assert_eq!(newer_paths(&tree.verdict()).len(), 1);
        tree.file("target/i686-pc-windows-msvc/release/higanrtool.dll", 250);
        assert_eq!(newer_paths(&tree.verdict()).len(), 1);
        tree.touch("build/bin/HiganRTool.dll", 250);
        assert_eq!(tree.verdict(), Verdict::Fresh);
        // `#[cfg(test)]` の中だけを直しても cargo は DLL を作り直さないが、それでも通る。
        tree.touch("src/dll/src/netplay/tests.rs", 300);
        assert_eq!(tree.verdict(), Verdict::Fresh);
    }

    #[test]
    fn missing_dep_info_or_listed_source_is_refused() {
        let tree = Tree::new("missing");
        let dll_d = tree.path("target/i686-pc-windows-msvc/release/higanrtool.d");
        std::fs::remove_file(&dll_d).unwrap();
        match tree.verdict() {
            Verdict::Stale(v) => {
                assert!(
                    matches!(&v[..], [Stale::DepInfo { path, .. }] if *path == dll_d),
                    "{v:?}"
                )
            }
            other => panic!("{other:?}"),
        }
        std::fs::write(&dll_d, "").unwrap();
        assert!(
            matches!(tree.verdict(), Verdict::Stale(v) if matches!(&v[..], [Stale::DepInfo { .. }]))
        );
        std::fs::write(&dll_d, "garbage without a separator\n").unwrap();
        assert!(
            matches!(tree.verdict(), Verdict::Stale(v) if matches!(&v[..], [Stale::DepInfo { .. }]))
        );
        std::fs::remove_file(&dll_d).unwrap();
        std::fs::create_dir(&dll_d).unwrap();
        assert!(
            matches!(tree.verdict(), Verdict::Stale(v) if matches!(&v[..], [Stale::DepInfo { .. }]))
        );

        let tree = Tree::new("gone");
        std::fs::remove_file(tree.path("src/loader/src/main.rs")).unwrap();
        match tree.verdict() {
            Verdict::Stale(v) => assert!(
                matches!(&v[..], [Stale::Source { violation: Violation::Missing(p), .. }]
                    if *p == tree.path("src/loader/src/main.rs")),
                "{v:?}"
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn not_dev_layouts_are_not_examined() {
        let tree = Tree::new("notdev");
        tree.touch("src/dll/src/lib.rs", 900);
        std::fs::remove_file(tree.path("target/i686-pc-windows-msvc/release/higanrtool.d"))
            .unwrap();
        assert!(matches!(tree.verdict(), Verdict::Stale(_)));

        let rel = tree.path("target/i686-pc-windows-msvc/release");
        assert_eq!(
            check(
                &rel.join("higanrtool_loader.exe"),
                &rel.join("higanrtool.dll")
            ),
            Verdict::NotDev
        );
        std::fs::rename(tree.path("src"), tree.path("src_away")).unwrap();
        assert_eq!(tree.verdict(), Verdict::NotDev);
        std::fs::rename(tree.path("src_away"), tree.path("src")).unwrap();
        std::fs::remove_file(tree.path("Cargo.toml")).unwrap();
        assert_eq!(tree.verdict(), Verdict::NotDev);
    }

    #[test]
    fn description_names_the_file_and_both_times() {
        let s = Stale::Source {
            artifact: PathBuf::from(r"C:\R\build\bin\HiganRTool.dll"),
            artifact_time: t(0),
            violation: Violation::Newer(PathBuf::from(r"C:\R\src\dll\src\lib.rs"), t(75)),
        };
        let msg = describe(&s);
        assert!(msg.contains(r"C:\R\src\dll\src\lib.rs"), "{msg}");
        assert!(msg.contains(r"C:\R\build\bin\HiganRTool.dll"), "{msg}");
        assert!(
            msg.contains(&fmt_utc(t(0))) && msg.contains(&fmt_utc(t(75))),
            "{msg}"
        );
        assert!(msg.contains("75 秒"), "{msg}");
    }

    #[test]
    fn dep_info_names_match_the_manifests_and_build_bat() {
        let dll_toml = include_str!("../../dll/Cargo.toml");
        let loader_toml = include_str!("../../loader/Cargo.toml");
        let config = include_str!("../../../.cargo/config.toml");
        let build_bat = include_str!("../../../build.bat");
        let stem = |d: &str| d.strip_suffix(".d").unwrap().to_string();
        assert!(dll_toml.contains(&format!("name = \"{}\"", stem(DLL_DEP_INFO))));
        assert!(loader_toml.contains(&format!("name = \"{}\"", stem(LOADER_DEP_INFO))));
        assert!(config.contains(&format!("target = \"{}\"", RELEASE_DIR[1])));
        assert!(build_bat.contains(&format!("{}\\%TARGET%\\{}", RELEASE_DIR[0], RELEASE_DIR[2])));
        assert!(build_bat.contains(&format!("{}.dll", stem(DLL_DEP_INFO))));
        assert!(build_bat.contains(&format!("{}.exe", stem(LOADER_DEP_INFO))));
    }

    #[test]
    #[ignore = "build.bat の後に手で回す（build\\bin の実物を読む）"]
    fn built_build_bin_is_fresh() {
        // `absolute` は `..` を畳む（`canonicalize` と違い `\\?\` を付けない）。
        let root =
            std::path::absolute(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
        let bin = root.join("build").join("bin");
        let v = check(&bin.join("HiganRTool.exe"), &bin.join("HiganRTool.dll"));
        if let Verdict::Stale(list) = &v {
            for s in list {
                eprintln!("{}", describe(s));
            }
        }
        assert_eq!(v, Verdict::Fresh);
    }
}
