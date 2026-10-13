//! `manifest_sign` —— 配る側の道具。更新の目録の署名の鍵を作り、目録に署名し、確かめる。配る exe・dll には入らない。
//!
//! ```text
//! manifest_sign keygen --key <秘密の鍵のファイル>              新しい鍵を作る。公開鍵（16 進）を出す
//! manifest_sign pubkey --key <秘密の鍵のファイル>              公開鍵（16 進）を出す
//! manifest_sign sign   --key <秘密の鍵のファイル> --in <署名なしの目録> --out <署名つきの目録>
//! manifest_sign verify --pub <公開鍵の 16 進> --in <署名つきの目録>
//! ```
//!
//! 鍵のファイルは git に追跡されない場所に置く（作業ツリーの中で `.gitignore` に外されていない場所と `dist` の下は
//! 読み書きとも断る）。中身は種 32 バイトの
//! 小文字の 16 進。無くすと配った exe へ更新を届けられないのでバックアップを取る。`keygen` は既存のファイルを上書きしない。
//! 署名の前に本文の形を確かめ、署名の後に公開鍵で確かめ直してから書く。公開鍵は `src/core/src/update.rs` の
//! `UPDATE_PUBLIC_KEY_HEX` に貼る。

use std::io::Write;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey};
use higanr_core::update;

const USAGE: &str = "\
使い方:
  manifest_sign keygen --key <秘密の鍵のファイル>
  manifest_sign pubkey --key <秘密の鍵のファイル>
  manifest_sign sign   --key <秘密の鍵のファイル> --in <署名なしの目録> --out <署名つきの目録>
  manifest_sign verify --pub <公開鍵の16進> --in <署名つきの目録>
鍵のファイルは dist フォルダの外に置く。リポジトリの中なら .gitignore で外した場所（keys\\ など）だけ。";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = std::io::stdout();
    match run(&args, &mut out) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("manifest_sign: {e}");
            std::process::exit(1);
        }
    }
}

fn run(args: &[String], out: &mut dyn Write) -> Result<(), String> {
    let Some((cmd, rest)) = args.split_first() else {
        return Err(USAGE.to_string());
    };
    let opts = Opts::parse(rest)?;
    match cmd.as_str() {
        "keygen" => keygen(&opts.need("--key")?, out),
        "pubkey" => {
            let key = read_key(&opts.need("--key")?)?;
            writeln!(out, "{}", update::to_hex(&key.verifying_key().to_bytes())).map_err(io)
        }
        "sign" => sign(
            &opts.need("--key")?,
            &opts.need("--in")?,
            &opts.need("--out")?,
            out,
        ),
        "verify" => verify(&opts.need("--pub")?, &opts.need("--in")?, out),
        other => Err(format!("知らない副命令 {other:?}\n{USAGE}")),
    }
}

fn io(e: std::io::Error) -> String {
    e.to_string()
}

struct Opts(Vec<(String, String)>);

impl Opts {
    fn parse(args: &[String]) -> Result<Opts, String> {
        let mut v: Vec<(String, String)> = Vec::new();
        let mut it = args.iter();
        while let Some(k) = it.next() {
            if !k.starts_with("--") {
                return Err(format!("{k:?} は --名前 の形でない\n{USAGE}"));
            }
            let val = it
                .next()
                .ok_or_else(|| format!("{k} の後に値がありません"))?;
            if v.iter().any(|(n, _)| n == k) {
                return Err(format!("{k} が 2 回あります"));
            }
            v.push((k.clone(), val.clone()));
        }
        Ok(Opts(v))
    }

    fn need(&self, name: &str) -> Result<String, String> {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| format!("{name} が要ります\n{USAGE}"))
    }
}

/// `dist` の下か、リポジトリの中で git に無視されていない場所ならその理由。
fn forbidden_key_place(path: &Path) -> Option<String> {
    let abs = std::path::absolute(path).ok()?;
    let mut here: Option<&Path> = abs.parent();
    while let Some(dir) = here {
        if dir.join(".git").exists() {
            if git_ignores(dir, &abs) {
                return None;
            }
            return Some(format!(
                "{} は git のリポジトリ（{}）の中で、.gitignore で外されていません",
                abs.display(),
                dir.display()
            ));
        }
        if dir
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("dist"))
        {
            return Some(format!(
                "{} は dist フォルダ（配る物の置き場）の下です",
                abs.display()
            ));
        }
        here = dir.parent();
    }
    None
}

/// git が `path` を無視するか。git を呼べなければ無視しないとみなす（断る側に倒す）。
fn git_ignores(repo: &Path, path: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "-q", "--"])
        .arg(path)
        .status()
        .is_ok_and(|s| s.success())
}

fn check_key_place(path: &Path) -> Result<(), String> {
    match forbidden_key_place(path) {
        Some(why) => Err(format!(
            "秘密の鍵のファイルをここに置いたり読んだりはしません: {why}。dist の外で、リポジトリの中なら .gitignore で外した場所のパスを渡してください"
        )),
        None => Ok(()),
    }
}

fn keygen(path: &str, out: &mut dyn Write) -> Result<(), String> {
    let path = PathBuf::from(path);
    check_key_place(&path)?;
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| format!("OS の乱数を引けません（{e}）"))?;
    let text = format!("{}\n", update::to_hex(&seed));
    // create_new: 既にある鍵を上書きしない（無くすと配った exe へ更新を届けられなくなる）。
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            format!(
                "{} を新しく作れません（{e}）。既にあるファイルは上書きしません",
                path.display()
            )
        })?;
    f.write_all(text.as_bytes()).map_err(io)?;
    f.sync_all().map_err(io)?;
    let public = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
    writeln!(out, "秘密の鍵を {} に書きました。バックアップを別の場所にも取ってください（無くすと更新を届けられません）。", path.display()).map_err(io)?;
    writeln!(
        out,
        "公開鍵（src/core/src/update.rs の UPDATE_PUBLIC_KEY_HEX に貼る）:"
    )
    .map_err(io)?;
    writeln!(out, "{}", update::to_hex(&public)).map_err(io)
}

fn read_key(path: &str) -> Result<SigningKey, String> {
    let path = PathBuf::from(path);
    check_key_place(&path)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("{} を読めません（{e}）", path.display()))?;
    let mut seed = [0u8; 32];
    update::from_hex_exact(text.trim_end_matches(['\r', '\n']), &mut seed)
        .ok_or_else(|| format!("{} は小文字の16進64文字の1行ではありません", path.display()))?;
    Ok(SigningKey::from_bytes(&seed))
}

fn sign(key: &str, input: &str, output: &str, out: &mut dyn Write) -> Result<(), String> {
    let key = read_key(key)?;
    let body =
        std::fs::read_to_string(input).map_err(|e| format!("{input} を読めません（{e}）"))?;
    // 壊れた目録に署名しない（読み手が断る目録を配らない）。
    let m = update::parse_body(&body).map_err(|e| format!("{input} の形が使えません: {e}"))?;
    let sig = key.sign(&update::signing_message(body.as_bytes()));
    let signed = update::compose_signed(&body, &sig.to_bytes()).map_err(|e| e.to_string())?;
    // 書く前に、公開鍵で確かめ直す（署名の取り違え・書式の食い違いをここで止める）。
    let public = key.verifying_key().to_bytes();
    let back = update::verify_and_parse(signed.as_bytes(), &public)
        .map_err(|e| format!("署名したものを確かめ直せませんでした: {e}"))?;
    if back != m {
        return Err("署名したものを読み直すと中身が違います".to_string());
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|e| {
            format!("{output} を新しく作れません（{e}）。既にあるファイルは上書きしません")
        })?;
    f.write_all(signed.as_bytes()).map_err(io)?;
    writeln!(
        out,
        "署名しました: {output}（ビルド番号 {}、表示 {}、ファイル {} 個、公開鍵 {}）",
        m.version,
        m.display,
        m.files.len(),
        update::to_hex(&public)
    )
    .map_err(io)
}

fn verify(public_hex: &str, input: &str, out: &mut dyn Write) -> Result<(), String> {
    let mut public = [0u8; 32];
    update::from_hex_exact(public_hex, &mut public)
        .ok_or_else(|| "--pub は小文字の16進64文字".to_string())?;
    let bytes = std::fs::read(input).map_err(|e| format!("{input} を読めません（{e}）"))?;
    let m = update::verify_and_parse(&bytes, &public).map_err(|e| e.to_string())?;
    writeln!(
        out,
        "署名は合っています（ビルド番号 {}、表示 {}、ファイル {} 個）",
        m.version,
        m.display,
        m.files.len()
    )
    .map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "HiganRTool_manifest_sign_{}_{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("一時フォルダ");
        dir
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn body() -> String {
        let sha = update::to_hex(&update::sha256(b"x"));
        format!(
            "{}\nversion 42\ndisplay 20261008-abcdef0\nfile HiganRTool.exe 10 {sha}\nfile HiganRTool.dll 10 {sha}\n",
            update::MANIFEST_MAGIC
        )
    }

    #[test]
    fn keygen_sign_verify_round_trip() {
        let dir = temp_dir("round");
        let (key, unsigned, signed) = (dir.join("k.txt"), dir.join("m.txt"), dir.join("s.txt"));
        std::fs::write(&unsigned, body()).unwrap();
        let mut out = Vec::new();
        run(&args(&["keygen", "--key", key.to_str().unwrap()]), &mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();
        let public = printed.lines().last().unwrap().to_string();
        assert_eq!(public.len(), 64, "{printed}");

        let mut out = Vec::new();
        run(&args(&["pubkey", "--key", key.to_str().unwrap()]), &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().trim(), public);

        run(
            &args(&[
                "sign",
                "--key",
                key.to_str().unwrap(),
                "--in",
                unsigned.to_str().unwrap(),
                "--out",
                signed.to_str().unwrap(),
            ]),
            &mut Vec::new(),
        )
        .unwrap();
        let mut pk = [0u8; 32];
        update::from_hex_exact(&public, &mut pk).unwrap();
        let m = update::verify_and_parse(&std::fs::read(&signed).unwrap(), &pk)
            .expect("確かめる側が通す");
        assert_eq!(m.version, 42);
        run(
            &args(&["verify", "--pub", &public, "--in", signed.to_str().unwrap()]),
            &mut Vec::new(),
        )
        .unwrap();

        let other = SigningKey::from_bytes(&[3u8; 32])
            .verifying_key()
            .to_bytes();
        assert!(
            run(
                &args(&[
                    "verify",
                    "--pub",
                    &update::to_hex(&other),
                    "--in",
                    signed.to_str().unwrap()
                ]),
                &mut Vec::new()
            )
            .is_err()
        );
        let tampered = std::fs::read_to_string(&signed)
            .unwrap()
            .replace("version 42", "version 43");
        std::fs::write(dir.join("t.txt"), tampered).unwrap();
        assert!(
            run(
                &args(&[
                    "verify",
                    "--pub",
                    &public,
                    "--in",
                    dir.join("t.txt").to_str().unwrap()
                ]),
                &mut Vec::new()
            )
            .is_err()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_key_inside_a_repo_is_allowed_only_where_git_ignores_it() {
        let repo = temp_dir("repo");
        let git = |a: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(a)
                .output()
                .expect("git")
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join(".gitignore"), "/keys/\n/dist/\n").unwrap();
        assert!(forbidden_key_place(&repo.join("keys").join("k.txt")).is_none());
        let tracked = forbidden_key_place(&repo.join("k.txt"));
        assert!(tracked.is_some_and(|w| w.contains(".gitignore")));
        assert!(forbidden_key_place(&repo.join("src").join("k.txt")).is_some());
        // 無視されていても dist の下は断る。
        let dist = forbidden_key_place(&repo.join("dist").join("k.txt"));
        assert!(dist.is_some_and(|w| w.contains("dist")));
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn refuses_unsafe_places_overwrites_and_broken_bodies() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("never_written_key.txt");
        let e = run(
            &args(&["keygen", "--key", inside.to_str().unwrap()]),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(e.contains("リポジトリ"), "{e}");
        assert!(!inside.exists());
        assert!(forbidden_key_place(&inside).is_some());
        let dist = temp_dir("distroot").join("dist").join("k.txt");
        assert!(forbidden_key_place(&dist).is_some_and(|w| w.contains("dist")));
        assert!(forbidden_key_place(&temp_dir("ok").join("k.txt")).is_none());

        let dir = temp_dir("refuse");
        let key = dir.join("k.txt");
        run(
            &args(&["keygen", "--key", key.to_str().unwrap()]),
            &mut Vec::new(),
        )
        .unwrap();
        let before = std::fs::read(&key).unwrap();
        assert!(
            run(
                &args(&["keygen", "--key", key.to_str().unwrap()]),
                &mut Vec::new()
            )
            .is_err(),
            "上書きした"
        );
        assert_eq!(std::fs::read(&key).unwrap(), before);

        let unsigned = dir.join("m.txt");
        let signed = dir.join("s.txt");
        std::fs::write(&unsigned, body()).unwrap();
        let sign_args = args(&[
            "sign",
            "--key",
            key.to_str().unwrap(),
            "--in",
            unsigned.to_str().unwrap(),
            "--out",
            signed.to_str().unwrap(),
        ]);
        run(&sign_args, &mut Vec::new()).unwrap();
        let first = std::fs::read(&signed).unwrap();
        assert!(
            run(&sign_args, &mut Vec::new()).is_err(),
            "出力を上書きした"
        );
        assert_eq!(std::fs::read(&signed).unwrap(), first);

        for (what, bad) in [
            (
                "許可リスト外",
                body().replace("file HiganRTool.dll", "file ..\\evil.dll"),
            ),
            (
                "exe が無い",
                body().replace("file HiganRTool.exe", "file README.txt"),
            ),
            ("LF で終わらない", body().trim_end().to_string()),
        ] {
            let bad_in = dir.join("bad.txt");
            std::fs::write(&bad_in, bad).unwrap();
            let bad_out = dir.join(format!("bad_out_{}.txt", what.len()));
            let r = run(
                &args(&[
                    "sign",
                    "--key",
                    key.to_str().unwrap(),
                    "--in",
                    bad_in.to_str().unwrap(),
                    "--out",
                    bad_out.to_str().unwrap(),
                ]),
                &mut Vec::new(),
            );
            assert!(r.is_err(), "{what}: 署名した");
            assert!(!bad_out.exists(), "{what}: 書いた");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn argument_errors() {
        for bad in [
            vec![],
            vec!["frobnicate"],
            vec!["keygen"],
            vec!["keygen", "--key"],
            vec!["keygen", "--key", "a", "--key", "b"],
            vec!["sign", "--key", "a"],
            vec!["verify", "--pub", "zz", "--in", "x"],
            vec!["keygen", "positional"],
        ] {
            assert!(run(&args(&bad), &mut Vec::new()).is_err(), "{bad:?}");
        }
    }
}
