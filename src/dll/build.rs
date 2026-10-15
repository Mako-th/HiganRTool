//! 配る HiganRTool.dll にバージョン情報を付ける。

use higanr_build_res::{Artifact, FileKind, VersionInfo, emit};

fn main() {
    emit(
        &VersionInfo {
            file_description: "HiganRTool netplay module",
            internal_name: "HiganRTool",
            original_filename: "HiganRTool.dll",
            kind: FileKind::Dll,
        },
        Artifact::Cdylib,
    );
}
