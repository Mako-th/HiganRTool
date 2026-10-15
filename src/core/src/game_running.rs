//! 起動しようとしている th09.exe と同じパスの th09.exe がもう動いているか、を決める純粋な部分。
//! プロセスの一覧と名前つきミューテックスは Win32 の層（ローダーの `single`）が受け持つ。
//!
//! パスは大文字・小文字を区別せず、区切り・`.` / `..`・`\\?\` の接頭辞をそろえて比べる（NTFS は大文字・小文字を区別しない）。
//! 8.3 の短い名前までは解かない。呼ぶ側が `canonicalize` した絶対パスを渡す。

/// 比べるためのパスの形。`\\?\C:\a` → `c:\a`、`\\?\UNC\s\x` → `\\s\x`。
pub fn normalize_path(path: &str) -> String {
    let s = path.replace('/', "\\");
    let (unc, rest) = if let Some(r) = s.strip_prefix("\\\\?\\UNC\\") {
        (true, r)
    } else if let Some(r) = s.strip_prefix("\\\\?\\") {
        (false, r)
    } else if let Some(r) = s.strip_prefix("\\\\") {
        (true, r)
    } else {
        (false, s.as_str())
    };
    let mut parts: Vec<&str> = Vec::new();
    for c in rest.split('\\') {
        match c {
            "" | "." => {}
            // ドライブ名（先頭の要素）より上へは戻らない。
            ".." => {
                if parts.len() > 1 {
                    parts.pop();
                }
            }
            c => parts.push(c),
        }
    }
    let joined = parts.join("\\");
    let out = if unc { format!("\\\\{joined}") } else { joined };
    out.to_lowercase()
}

pub fn same_path(a: &str, b: &str) -> bool {
    normalize_path(a) == normalize_path(b)
}

/// パスの最後の要素（ファイル名）。区切りは `\` と `/`。
pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// プロセスの一覧（pid と実行ファイル名）から、`target` と同じ名前のものの pid を返す。自分は除く。
/// 名前で絞るのは、全プロセスのフルパスを引く（権限のないものもある）のを避けるため。
pub fn same_name_pids(procs: &[(u32, String)], target: &str, own_pid: u32) -> Vec<u32> {
    let want = file_name(target).to_lowercase();
    procs
        .iter()
        .filter(|(pid, name)| *pid != own_pid && name.to_lowercase() == want)
        .map(|(pid, _)| *pid)
        .collect()
}

/// ローダー同士が「確認から起動まで」を 1 つにするための名前つきミューテックスの名前。
/// 同じゲームの実行ファイルなら同じ名前、別のフォルダなら別の名前になる（検査用のコピーは並べて動かせる）。
/// `Local\` はセッションごと。名前に `\` は使えないので、パスは FNV-1a の 64 ビットにする。
pub fn lock_name(target: &str) -> String {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for b in normalize_path(target).bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("Local\\HiganRTool_Launch_{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = r"C:\Games\Touhou\th09.exe";

    #[test]
    fn paths_compare_without_case_separator_or_prefix_differences() {
        assert!(same_path(A, r"c:\games\TOUHOU\TH09.EXE"));
        assert!(same_path(A, r"\\?\C:\Games\Touhou\th09.exe"));
        assert!(same_path(A, "C:/Games/Touhou/th09.exe"));
        assert!(same_path(A, r"C:\Games\\Touhou\.\th09.exe"));
        assert!(same_path(A, r"C:\Games\Other\..\Touhou\th09.exe"));
        assert!(same_path(
            r"\\?\UNC\srv\share\th09.exe",
            r"\\SRV\Share\th09.exe"
        ));
    }

    #[test]
    fn different_folders_or_names_are_different_games() {
        assert!(!same_path(A, r"C:\Games\Touhou_b\th09.exe"));
        assert!(!same_path(A, r"C:\Games\Touhou\th09b.exe"));
        assert!(!same_path(A, r"D:\Games\Touhou\th09.exe"));
        // 先頭の区切りの数が違うだけの UNC とドライブは別物。
        assert!(!same_path(r"\\srv\th09.exe", r"srv\th09.exe"));
    }

    #[test]
    fn dot_dot_does_not_climb_above_the_drive() {
        assert_eq!(normalize_path(r"C:\..\..\a"), r"c:\a");
    }

    #[test]
    fn only_same_named_other_processes_are_candidates() {
        let procs = vec![
            (10, "TH09.exe".to_string()),
            (11, "th09.exe".to_string()),
            (12, "th09b.exe".to_string()),
            (13, "explorer.exe".to_string()),
        ];
        assert_eq!(same_name_pids(&procs, A, 99), vec![10, 11]);
        // 自分の pid は除く。
        assert_eq!(same_name_pids(&procs, A, 10), vec![11]);
        assert!(same_name_pids(&[], A, 1).is_empty());
        assert_eq!(file_name(A), "th09.exe");
        assert_eq!(file_name("th09.exe"), "th09.exe");
    }

    #[test]
    fn lock_name_follows_the_game_path_only() {
        let n = lock_name(A);
        assert_eq!(n, lock_name(r"\\?\c:\GAMES\touhou\TH09.exe"));
        assert_ne!(n, lock_name(r"C:\Games\Touhou_b\th09.exe"));
        assert!(n.starts_with("Local\\HiganRTool_Launch_"));
        // 名前に区切りの `\` は 1 つだけ（`Local\` の後ろ）。
        assert_eq!(n.matches('\\').count(), 1, "{n}");
    }
}
