//! 合成の最小の PE32（見出しだけ）を作る。`cargo test` の偽物のファイルと、自己検査の行 8（`pe::parse` は通るが
//! Windows のローダーは読めない DLL で `LoadLibraryW` に 0 を返させる）に使う。`pe::parse` が読む範囲だけ正しく置き、残りは 0。

const E_LFANEW: usize = 0x40;
const FILE_HEADER: usize = E_LFANEW + 4;
const OPTIONAL_HEADER: usize = FILE_HEADER + 20;
const SIZE_OF_OPTIONAL_HEADER: usize = 0xE0;
const SECTION_TABLE: usize = OPTIONAL_HEADER + SIZE_OF_OPTIONAL_HEADER;
/// `.text` の中身を置くファイル上の位置。
const TEXT_FILE_OFFSET: usize = 0x200;
const TEXT_RVA: u32 = 0x1000;

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// 入口は `.text` の先頭。`text_size_of_raw_data` は見出しに書くだけでファイルの長さは合わせない。`entry.len()` 以上を渡すこと。
pub fn pe32(time_date_stamp: u32, text_size_of_raw_data: u32, entry: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; TEXT_FILE_OFFSET + entry.len().max(0x10)];
    buf[0..2].copy_from_slice(b"MZ");
    put_u32(&mut buf, 0x3C, E_LFANEW as u32);
    buf[E_LFANEW..E_LFANEW + 4].copy_from_slice(b"PE\0\0");

    put_u16(&mut buf, FILE_HEADER, 0x014C); // machine: i386
    put_u16(&mut buf, FILE_HEADER + 2, 1); // 節の数
    put_u32(&mut buf, FILE_HEADER + 4, time_date_stamp);
    put_u16(&mut buf, FILE_HEADER + 16, SIZE_OF_OPTIONAL_HEADER as u16);
    // Characteristics: EXECUTABLE_IMAGE | 32BIT_MACHINE | DLL。DLL の見た目にしておく。
    put_u16(&mut buf, FILE_HEADER + 18, 0x2102);

    put_u16(&mut buf, OPTIONAL_HEADER, 0x10B); // PE32
    put_u32(&mut buf, OPTIONAL_HEADER + 16, TEXT_RVA); // AddressOfEntryPoint
    put_u32(&mut buf, OPTIONAL_HEADER + 28, 0x1000_0000); // ImageBase

    let s = SECTION_TABLE;
    buf[s..s + 5].copy_from_slice(b".text");
    put_u32(&mut buf, s + 8, 0x1000); // VirtualSize
    put_u32(&mut buf, s + 12, TEXT_RVA); // VirtualAddress
    put_u32(&mut buf, s + 16, text_size_of_raw_data); // SizeOfRawData
    put_u32(&mut buf, s + 20, TEXT_FILE_OFFSET as u32); // PointerToRawData

    buf[TEXT_FILE_OFFSET..TEXT_FILE_OFFSET + entry.len()].copy_from_slice(entry);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::pe::{self, Ident};

    #[test]
    fn synthetic_pe_parses_and_matches_its_own_ident() {
        let entry = [0x11, 0x22, 0x33, 0x44, 0x55];
        let bytes = pe32(0x1234_5678, 0x0040_0000, &entry);
        let info = pe::parse(&bytes).expect("core が読めること");
        assert_eq!(info.time_date_stamp, 0x1234_5678);
        let ident = Ident {
            time_date_stamp: 0x1234_5678,
            text_size_of_raw_data: 0x0040_0000,
            entry_bytes: &entry,
        };
        assert_eq!(pe::check_ident(&bytes, &ident), Ok(()));
        assert!(pe::check_ident(&bytes, &pe::TH09_V150A).is_err());
    }
}
