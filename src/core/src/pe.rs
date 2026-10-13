//! PE32 の見出しを読む（pure）。
//!
//! 32bit の PE32 だけを受け付ける（`MZ`・`PE\0\0`・machine `0x014C`・optional header の magic `0x10B`）。
//! PE32+ や x64 は `Err`。どの読み取りも長さを確かめてから行い、壊れたバイト列でもパニックしない。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeError {
    TooShortForDosHeader,
    NotMz,
    ELfanewOutOfRange,
    NotPeSignature,
    TooShortForFileHeader,
    UnsupportedMachine(u16),
    TooShortForOptionalHeader,
    Pe32Plus,
    UnsupportedMagic(u16),
    SectionOutOfRange { index: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionHeader {
    /// 生の 8 バイト（`.text` のように短ければ後ろは `0x00`）。
    pub name: [u8; 8],
    pub virtual_size: u32,
    pub virtual_address: u32,
    pub size_of_raw_data: u32,
    pub pointer_to_raw_data: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeInfo {
    pub time_date_stamp: u32,
    pub size_of_code: u32,
    pub address_of_entry_point: u32,
    pub image_base: u32,
    pub sections: Vec<SectionHeader>,
}

fn get_range(bytes: &[u8], off: usize, len: usize) -> Option<&[u8]> {
    let end = off.checked_add(len)?;
    bytes.get(off..end)
}

fn read_u16(bytes: &[u8], off: usize) -> Option<u16> {
    let s = get_range(bytes, off, 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32(bytes: &[u8], off: usize) -> Option<u32> {
    let s = get_range(bytes, off, 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

const MACHINE_I386: u16 = 0x014C;
const MAGIC_PE32: u16 = 0x10B;
const MAGIC_PE32_PLUS: u16 = 0x20B;

/// PE32 の見出しを読む。壊れた・切り詰められたバイト列は `Err` を返すだけでパニックしない。
pub fn parse(bytes: &[u8]) -> Result<PeInfo, PeError> {
    if bytes.len() < 0x40 {
        return Err(PeError::TooShortForDosHeader);
    }
    if &bytes[0..2] != b"MZ" {
        return Err(PeError::NotMz);
    }
    let e_lfanew = read_u32(bytes, 0x3C).ok_or(PeError::TooShortForDosHeader)? as usize;

    let pe_sig = get_range(bytes, e_lfanew, 4).ok_or(PeError::ELfanewOutOfRange)?;
    if pe_sig != b"PE\0\0" {
        return Err(PeError::NotPeSignature);
    }

    let file_header_off = e_lfanew + 4; // e_lfanew は上で境界内と確かめた u32 の値
    if get_range(bytes, file_header_off, 20).is_none() {
        return Err(PeError::TooShortForFileHeader);
    }
    let machine = read_u16(bytes, file_header_off).ok_or(PeError::TooShortForFileHeader)?;
    if machine != MACHINE_I386 {
        return Err(PeError::UnsupportedMachine(machine));
    }
    let number_of_sections =
        read_u16(bytes, file_header_off + 2).ok_or(PeError::TooShortForFileHeader)?;
    let time_date_stamp =
        read_u32(bytes, file_header_off + 4).ok_or(PeError::TooShortForFileHeader)?;
    let size_of_optional_header =
        read_u16(bytes, file_header_off + 16).ok_or(PeError::TooShortForFileHeader)? as usize;

    let optional_header_off = file_header_off + 20;
    if size_of_optional_header < 32 {
        return Err(PeError::TooShortForOptionalHeader);
    }
    if get_range(bytes, optional_header_off, size_of_optional_header).is_none() {
        return Err(PeError::TooShortForOptionalHeader);
    }
    let magic = read_u16(bytes, optional_header_off).ok_or(PeError::TooShortForOptionalHeader)?;
    if magic == MAGIC_PE32_PLUS {
        return Err(PeError::Pe32Plus);
    }
    if magic != MAGIC_PE32 {
        return Err(PeError::UnsupportedMagic(magic));
    }
    let size_of_code =
        read_u32(bytes, optional_header_off + 4).ok_or(PeError::TooShortForOptionalHeader)?;
    let address_of_entry_point =
        read_u32(bytes, optional_header_off + 16).ok_or(PeError::TooShortForOptionalHeader)?;
    let image_base =
        read_u32(bytes, optional_header_off + 28).ok_or(PeError::TooShortForOptionalHeader)?;

    let section_table_off = optional_header_off + size_of_optional_header;
    let mut sections = Vec::with_capacity(number_of_sections as usize);
    for index in 0..number_of_sections as usize {
        let off = section_table_off
            .checked_add(index * 40)
            .ok_or(PeError::SectionOutOfRange { index })?;
        let raw = get_range(bytes, off, 40).ok_or(PeError::SectionOutOfRange { index })?;
        let mut name = [0u8; 8];
        name.copy_from_slice(&raw[0..8]);
        let virtual_size = read_u32(bytes, off + 8).ok_or(PeError::SectionOutOfRange { index })?;
        let virtual_address =
            read_u32(bytes, off + 12).ok_or(PeError::SectionOutOfRange { index })?;
        let size_of_raw_data =
            read_u32(bytes, off + 16).ok_or(PeError::SectionOutOfRange { index })?;
        let pointer_to_raw_data =
            read_u32(bytes, off + 20).ok_or(PeError::SectionOutOfRange { index })?;
        sections.push(SectionHeader {
            name,
            virtual_size,
            virtual_address,
            size_of_raw_data,
            pointer_to_raw_data,
        });
    }

    Ok(PeInfo {
        time_date_stamp,
        size_of_code,
        address_of_entry_point,
        image_base,
        sections,
    })
}

fn trim_trailing_nul(name: &[u8; 8]) -> &[u8] {
    let end = name.iter().position(|&b| b == 0).unwrap_or(8);
    &name[..end]
}

impl PeInfo {
    pub fn section_by_name(&self, name: &str) -> Option<&SectionHeader> {
        let target = name.as_bytes();
        self.sections
            .iter()
            .find(|s| trim_trailing_nul(&s.name) == target)
    }

    /// RVA をファイル上の位置に変換する。どの節にも属さない、またはファイル実体の無い範囲（BSS 相当）なら `None`。
    pub fn rva_to_file_offset(&self, rva: u32) -> Option<u32> {
        for s in &self.sections {
            // BSS 相当の領域も「その節に属する RVA」として扱うため、範囲の判定には virtual_size を使う。
            let va_end = s.virtual_address.checked_add(s.virtual_size)?;
            if rva >= s.virtual_address && rva < va_end {
                let delta = rva - s.virtual_address;
                if delta >= s.size_of_raw_data {
                    return None; // BSS 相当。ファイル上に実体が無い
                }
                return s.pointer_to_raw_data.checked_add(delta);
            }
        }
        None
    }

    /// 入口（`address_of_entry_point`）から `n` バイト。`image` は `PeInfo` を作った元のバイト列。
    pub fn entry_bytes<'a>(&self, image: &'a [u8], n: usize) -> Option<&'a [u8]> {
        let file_off = self.rva_to_file_offset(self.address_of_entry_point)? as usize;
        get_range(image, file_off, n)
    }
}

/// ゲームの同定に使う組（thprac と同じ材料: timeStamp / textSize / 入口のバイト列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ident<'a> {
    pub time_date_stamp: u32,
    /// `.text` 節の SizeOfRawData。optional header の SizeOfCode ではない（1.50a では偶然同じ値）。
    pub text_size_of_raw_data: u32,
    pub entry_bytes: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentField {
    TimeDateStamp,
    TextSizeOfRawData,
    EntryBytes,
}

/// 花映塚 1.50a の入口（RVA `0x0007D45F`、ファイル位置 `0x0007C85F`）から 20 バイト（実機の exe を読み取り専用で読んだ）。
/// 20 バイトは thprac が同定に使う長さ。
pub const TH09_V150A_ENTRY_BYTES: [u8; 20] = [
    0x6A, 0x60, 0x68, 0xA8, 0x55, 0x49, 0x00, 0xE8, 0x45, 0x1C, 0x00, 0x00, 0xBF, 0x94, 0x00, 0x00,
    0x00, 0x8B, 0xC7, 0xE8,
];

/// 花映塚 1.50a の同定の値。
pub const TH09_V150A: Ident<'static> = Ident {
    time_date_stamp: 1_128_708_539,     // 0x4346B9BB
    text_size_of_raw_data: 0x0008_CC00, // 576,512
    entry_bytes: &TH09_V150A_ENTRY_BYTES,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentMismatch {
    pub fields: Vec<IdentField>,
}

/// `bytes` が `ident` の指すゲーム本体と一致するか。PE32 として読めなければ 3 欄すべてを食い違いとして返す。
pub fn check_ident(bytes: &[u8], ident: &Ident<'_>) -> Result<(), IdentMismatch> {
    let info = match parse(bytes) {
        Ok(info) => info,
        Err(_) => {
            return Err(IdentMismatch {
                fields: vec![
                    IdentField::TimeDateStamp,
                    IdentField::TextSizeOfRawData,
                    IdentField::EntryBytes,
                ],
            });
        }
    };

    let mut fields = Vec::new();
    if info.time_date_stamp != ident.time_date_stamp {
        fields.push(IdentField::TimeDateStamp);
    }
    // `.text` が無い exe は花映塚ではないので、既定値 0 と比べて素通りさせず食い違いに数える。
    let text_size = info.section_by_name(".text").map(|s| s.size_of_raw_data);
    if text_size != Some(ident.text_size_of_raw_data) {
        fields.push(IdentField::TextSizeOfRawData);
    }
    let entry_matches = info
        .entry_bytes(bytes, ident.entry_bytes.len())
        .is_some_and(|actual| actual == ident.entry_bytes);
    if !entry_matches {
        fields.push(IdentField::EntryBytes);
    }

    if fields.is_empty() {
        Ok(())
    } else {
        Err(IdentMismatch { fields })
    }
}

#[cfg(test)]
mod tests {
    //! 実物の exe は使わず、合成の最小 PE32（データディレクトリ無しの 96 バイトの optional header）を組む。
    use super::*;

    const E_LFANEW: usize = 0x40;
    const FILE_HEADER_OFF: usize = E_LFANEW + 4; // 0x44
    const OPTIONAL_HEADER_OFF: usize = FILE_HEADER_OFF + 20; // 0x58
    const SIZE_OF_OPTIONAL_HEADER: usize = 96; // データディレクトリ無し
    const SECTION_TABLE_OFF: usize = OPTIONAL_HEADER_OFF + SIZE_OF_OPTIONAL_HEADER; // 0xB8
    const SECTION_HEADER_LEN: usize = 40;

    struct Builder {
        buf: Vec<u8>,
    }

    impl Builder {
        fn new(size: usize) -> Self {
            Self {
                buf: vec![0u8; size],
            }
        }
        fn set_u16(&mut self, off: usize, v: u16) {
            self.buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
        }
        fn set_u32(&mut self, off: usize, v: u32) {
            self.buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        fn set_bytes(&mut self, off: usize, b: &[u8]) {
            self.buf[off..off + b.len()].copy_from_slice(b);
        }
    }

    struct SectionSpec<'a> {
        name: &'a [u8],
        virtual_address: u32,
        virtual_size: u32,
        pointer_to_raw_data: u32,
        size_of_raw_data: u32,
        raw_data: &'a [u8],
    }

    fn build_minimal_pe32(
        time_date_stamp: u32,
        size_of_code: u32,
        entry_rva: u32,
        sections: &[SectionSpec],
    ) -> Vec<u8> {
        let file_end = sections
            .iter()
            .map(|s| s.pointer_to_raw_data as usize + s.raw_data.len())
            .max()
            .unwrap_or(SECTION_TABLE_OFF + sections.len() * SECTION_HEADER_LEN);
        let mut b = Builder::new(file_end);

        b.set_bytes(0, b"MZ");
        b.set_u32(0x3C, E_LFANEW as u32);

        b.set_bytes(E_LFANEW, b"PE\0\0");

        b.set_u16(FILE_HEADER_OFF, MACHINE_I386);
        b.set_u16(FILE_HEADER_OFF + 2, sections.len() as u16);
        b.set_u32(FILE_HEADER_OFF + 4, time_date_stamp);
        b.set_u16(FILE_HEADER_OFF + 16, SIZE_OF_OPTIONAL_HEADER as u16);

        b.set_u16(OPTIONAL_HEADER_OFF, MAGIC_PE32);
        b.set_u32(OPTIONAL_HEADER_OFF + 4, size_of_code);
        b.set_u32(OPTIONAL_HEADER_OFF + 16, entry_rva);
        b.set_u32(OPTIONAL_HEADER_OFF + 28, 0x0040_0000); // image_base

        for (i, s) in sections.iter().enumerate() {
            let off = SECTION_TABLE_OFF + i * SECTION_HEADER_LEN;
            let mut name8 = [0u8; 8];
            name8[..s.name.len()].copy_from_slice(s.name);
            b.set_bytes(off, &name8);
            b.set_u32(off + 8, s.virtual_size);
            b.set_u32(off + 12, s.virtual_address);
            b.set_u32(off + 16, s.size_of_raw_data);
            b.set_u32(off + 20, s.pointer_to_raw_data);
            b.set_bytes(s.pointer_to_raw_data as usize, s.raw_data);
        }

        b.buf
    }

    fn one_section_image(time_date_stamp: u32, size_of_code: u32, entry_bytes: &[u8]) -> Vec<u8> {
        let entry_rva = 0x1000u32;
        let raw = {
            let mut v = vec![0u8; 16.max(entry_bytes.len())];
            v[..entry_bytes.len()].copy_from_slice(entry_bytes);
            v
        };
        build_minimal_pe32(
            time_date_stamp,
            size_of_code,
            entry_rva,
            &[SectionSpec {
                name: b".text",
                virtual_address: entry_rva,
                virtual_size: raw.len() as u32,
                pointer_to_raw_data: 0x200,
                size_of_raw_data: raw.len() as u32,
                raw_data: &raw,
            }],
        )
    }

    #[test]
    fn parse_reads_header_fields_of_a_minimal_image() {
        let entry = [0xAAu8, 0xBB, 0xCC, 0xDD];
        let image = one_section_image(0x1234_5678, 0x0000_2000, &entry);
        let info = parse(&image).expect("minimal image must parse");
        assert_eq!(info.time_date_stamp, 0x1234_5678);
        assert_eq!(info.size_of_code, 0x0000_2000);
        assert_eq!(info.address_of_entry_point, 0x1000);
        assert_eq!(info.image_base, 0x0040_0000);
        assert_eq!(info.sections.len(), 1);
    }

    #[test]
    fn section_by_name_finds_text_and_ignores_trailing_nul() {
        let image = one_section_image(1, 1, &[0x90]);
        let info = parse(&image).unwrap();
        let text = info.section_by_name(".text").expect(".text must be found");
        assert_eq!(text.virtual_address, 0x1000);
        assert!(info.section_by_name(".data").is_none());
    }

    #[test]
    fn entry_bytes_reads_the_bytes_at_the_entry_point() {
        let entry = [0x55u8, 0x8B, 0xEC, 0x90];
        let image = one_section_image(1, 1, &entry);
        let info = parse(&image).unwrap();
        assert_eq!(info.entry_bytes(&image, 4), Some(&entry[..]));
    }

    #[test]
    fn two_sections_are_both_readable_by_name() {
        let text_raw = vec![0x90u8; 16];
        let data_raw = vec![0x00u8; 16];
        let image = build_minimal_pe32(
            0xAAAA_BBBB,
            0x1000,
            0x1000,
            &[
                SectionSpec {
                    name: b".text",
                    virtual_address: 0x1000,
                    virtual_size: 16,
                    pointer_to_raw_data: 0x200,
                    size_of_raw_data: 16,
                    raw_data: &text_raw,
                },
                SectionSpec {
                    name: b".data",
                    virtual_address: 0x2000,
                    virtual_size: 16,
                    pointer_to_raw_data: 0x210,
                    size_of_raw_data: 16,
                    raw_data: &data_raw,
                },
            ],
        );
        let info = parse(&image).expect("two-section image must parse");
        assert_eq!(info.sections.len(), 2);
        assert!(info.section_by_name(".text").is_some());
        assert!(info.section_by_name(".data").is_some());
    }

    #[test]
    fn check_ident_ok_when_all_three_fields_match() {
        let entry = [0x11u8, 0x22, 0x33, 0x44];
        // SizeOfCode と `.text` の SizeOfRawData をわざと違う値にしてある。
        let image = one_section_image(0x5000_0000, 0x3000, &entry);
        let ident = Ident {
            time_date_stamp: 0x5000_0000,
            text_size_of_raw_data: 16,
            entry_bytes: &entry,
        };
        assert_eq!(check_ident(&image, &ident), Ok(()));
    }

    #[test]
    fn check_ident_compares_text_size_of_raw_data_not_size_of_code() {
        let entry = [0x11u8, 0x22, 0x33, 0x44];
        let image = one_section_image(0x5000_0000, 0x3000, &entry);
        let info = parse(&image).unwrap();
        assert_eq!(info.size_of_code, 0x3000);
        assert_eq!(info.section_by_name(".text").unwrap().size_of_raw_data, 16);

        let by_text = Ident {
            time_date_stamp: 0x5000_0000,
            text_size_of_raw_data: 16,
            entry_bytes: &entry,
        };
        assert_eq!(check_ident(&image, &by_text), Ok(()));

        let by_size_of_code = Ident {
            text_size_of_raw_data: 0x3000,
            ..by_text
        };
        assert_eq!(
            check_ident(&image, &by_size_of_code),
            Err(IdentMismatch {
                fields: vec![IdentField::TextSizeOfRawData]
            })
        );
    }

    #[test]
    fn check_ident_reports_missing_text_section() {
        let raw = vec![0x90u8; 16];
        let image = build_minimal_pe32(
            7,
            0,
            0x1000,
            &[SectionSpec {
                name: b"CODE",
                virtual_address: 0x1000,
                virtual_size: 16,
                pointer_to_raw_data: 0x200,
                size_of_raw_data: 16,
                raw_data: &raw,
            }],
        );
        let ident = Ident {
            time_date_stamp: 7,
            text_size_of_raw_data: 0,
            entry_bytes: &[0x90, 0x90],
        };
        assert_eq!(
            check_ident(&image, &ident),
            Err(IdentMismatch {
                fields: vec![IdentField::TextSizeOfRawData]
            })
        );
    }

    #[test]
    fn th09_entry_bytes_match_thprac_xor_words() {
        const THPRAC_OEP_WORDS: [u16; 10] = [
            0x212B, 0xEA2A, 0x0A16, 0xAC44, 0x5900, 0x4646, 0xD3F8, 0x4848, 0xC249, 0xA28D,
        ];
        let b = TH09_V150A.entry_bytes;
        assert_eq!(b.len(), 20);
        for (i, want) in THPRAC_OEP_WORDS.iter().enumerate() {
            let word = u16::from_le_bytes([b[2 * i], b[2 * i + 1]]);
            let k = (i as u16) + 0x41;
            assert_eq!(word ^ (k << 8 | k), *want, "word {i}");
        }
        assert_eq!(TH09_V150A.time_date_stamp, 1_128_708_539);
        assert_eq!(TH09_V150A.text_size_of_raw_data, 576_512);
    }

    #[test]
    fn th09_ident_accepts_a_synthetic_image_with_the_same_layout() {
        let entry_rva = 0x0007_D45Fu32;
        let text_va = 0x1000u32;
        let delta = (entry_rva - text_va) as usize;
        let mut raw = vec![0xCCu8; delta + 32];
        raw[delta..delta + 20].copy_from_slice(&TH09_V150A_ENTRY_BYTES);
        let image = build_minimal_pe32(
            TH09_V150A.time_date_stamp,
            0x1234, // SizeOfCode はわざと違う値
            entry_rva,
            &[SectionSpec {
                name: b".text",
                virtual_address: text_va,
                virtual_size: raw.len() as u32,
                pointer_to_raw_data: 0x400,
                size_of_raw_data: TH09_V150A.text_size_of_raw_data,
                raw_data: &raw,
            }],
        );
        assert_eq!(check_ident(&image, &TH09_V150A), Ok(()));

        let mut poisoned = image.clone();
        poisoned[0x400 + delta + 19] ^= 0xFF;
        assert_eq!(
            check_ident(&poisoned, &TH09_V150A),
            Err(IdentMismatch {
                fields: vec![IdentField::EntryBytes]
            })
        );
    }

    #[test]
    fn parse_rejects_wrong_dos_magic() {
        let mut image = one_section_image(1, 1, &[0x90]);
        image[0] = b'X'; // "MZ" -> "XZ"
        assert_eq!(parse(&image), Err(PeError::NotMz));
    }

    #[test]
    fn parse_rejects_e_lfanew_out_of_range() {
        let mut image = one_section_image(1, 1, &[0x90]);
        image[0x3C..0x40].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert_eq!(parse(&image), Err(PeError::ELfanewOutOfRange));
    }

    #[test]
    fn parse_rejects_wrong_pe_signature() {
        let mut image = one_section_image(1, 1, &[0x90]);
        image[E_LFANEW] = b'X'; // "PE\0\0" -> "XE\0\0"
        assert_eq!(parse(&image), Err(PeError::NotPeSignature));
    }

    #[test]
    fn parse_rejects_x64_machine() {
        let mut image = one_section_image(1, 1, &[0x90]);
        image[FILE_HEADER_OFF..FILE_HEADER_OFF + 2].copy_from_slice(&0x8664u16.to_le_bytes());
        assert_eq!(parse(&image), Err(PeError::UnsupportedMachine(0x8664)));
    }

    #[test]
    fn parse_rejects_pe32_plus_magic() {
        let mut image = one_section_image(1, 1, &[0x90]);
        image[OPTIONAL_HEADER_OFF..OPTIONAL_HEADER_OFF + 2]
            .copy_from_slice(&MAGIC_PE32_PLUS.to_le_bytes());
        assert_eq!(parse(&image), Err(PeError::Pe32Plus));
    }

    #[test]
    fn entry_point_outside_any_section_yields_none_not_panic() {
        let entry_rva = 0x9000u32;
        let raw = vec![0x90u8; 16];
        let image = build_minimal_pe32(
            1,
            1,
            entry_rva,
            &[SectionSpec {
                name: b".text",
                virtual_address: 0x1000,
                virtual_size: 16,
                pointer_to_raw_data: 0x200,
                size_of_raw_data: 16,
                raw_data: &raw,
            }],
        );
        let info = parse(&image).expect("header itself is still valid");
        assert_eq!(info.rva_to_file_offset(entry_rva), None);
        assert_eq!(info.entry_bytes(&image, 4), None);

        let ident = Ident {
            time_date_stamp: 1,
            text_size_of_raw_data: 16,
            entry_bytes: &[0x90, 0x90, 0x90, 0x90],
        };
        let err = check_ident(&image, &ident).unwrap_err();
        assert!(err.fields.contains(&IdentField::EntryBytes));
    }

    #[test]
    fn check_ident_reports_every_mismatched_field() {
        let entry = [0x11u8, 0x22, 0x33, 0x44];
        let image = one_section_image(0x1111_1111, 0x2222, &entry);
        let ident = Ident {
            time_date_stamp: 0x9999_9999,           // 違う
            text_size_of_raw_data: 16,              // 同じ（`.text` は 16 バイト）
            entry_bytes: &[0xFF, 0xFF, 0xFF, 0xFF], // 違う
        };
        let err = check_ident(&image, &ident).unwrap_err();
        assert!(err.fields.contains(&IdentField::TimeDateStamp));
        assert!(!err.fields.contains(&IdentField::TextSizeOfRawData));
        assert!(err.fields.contains(&IdentField::EntryBytes));
    }

    #[test]
    fn parse_never_panics_on_any_truncation_of_a_valid_image() {
        let entry = [0x11u8, 0x22, 0x33, 0x44];
        let image = one_section_image(0x1234_5678, 0x1000, &entry);
        for len in 0..=image.len() {
            let _ = parse(&image[..len]);
        }
    }

    #[test]
    fn parse_never_panics_on_any_truncation_of_a_two_section_image() {
        let text_raw = vec![0x90u8; 16];
        let data_raw = vec![0x00u8; 16];
        let image = build_minimal_pe32(
            1,
            1,
            0x1000,
            &[
                SectionSpec {
                    name: b".text",
                    virtual_address: 0x1000,
                    virtual_size: 16,
                    pointer_to_raw_data: 0x200,
                    size_of_raw_data: 16,
                    raw_data: &text_raw,
                },
                SectionSpec {
                    name: b".data",
                    virtual_address: 0x2000,
                    virtual_size: 16,
                    pointer_to_raw_data: 0x210,
                    size_of_raw_data: 16,
                    raw_data: &data_raw,
                },
            ],
        );
        for len in 0..=image.len() {
            let _ = parse(&image[..len]);
        }
    }
}
