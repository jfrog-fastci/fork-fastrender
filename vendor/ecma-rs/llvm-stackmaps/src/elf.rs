//! Minimal ELF64 section reader.
//!
//! This is used by tests/debug tooling to extract stackmaps from an
//! object/executable file.
//!
//! LLVM typically emits stackmaps into the `.llvm_stackmaps` section in object
//! files. When linking PIE binaries, this repository's linker-script fragment
//! moves the bytes into a RELRO-friendly output section:
//! `.data.rel.ro.llvm_stackmaps` (see `runtime-native/link/stackmaps.ld`).
//!
//! The runtime path uses linker-provided start/end symbols instead (see
//! [`crate::stackmap::stackmaps_bytes`]).

use std::fmt;

#[derive(Debug, Clone)]
pub struct ElfError {
    pub message: &'static str,
}

impl fmt::Display for ElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for ElfError {}

fn u16_le(bytes: &[u8], offset: usize) -> Result<u16, ElfError> {
    let b = bytes
        .get(offset..offset + 2)
        .ok_or(ElfError { message: "ELF: truncated u16" })?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_le(bytes: &[u8], offset: usize) -> Result<u32, ElfError> {
    let b = bytes
        .get(offset..offset + 4)
        .ok_or(ElfError { message: "ELF: truncated u32" })?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_le(bytes: &[u8], offset: usize) -> Result<u64, ElfError> {
    let b = bytes
        .get(offset..offset + 8)
        .ok_or(ElfError { message: "ELF: truncated u64" })?;
    Ok(u64::from_le_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

fn get_cstr<'a>(bytes: &'a [u8], offset: usize) -> Result<&'a str, ElfError> {
    let bytes = bytes
        .get(offset..)
        .ok_or(ElfError { message: "ELF: string table offset out of range" })?;
    let len = bytes
        .iter()
        .position(|&b| b == 0)
        .ok_or(ElfError { message: "ELF: unterminated string in strtab" })?;
    std::str::from_utf8(&bytes[..len]).map_err(|_| ElfError {
        message: "ELF: non-utf8 section name",
    })
}

/// Return the raw bytes for an ELF section by name.
///
/// Supports 64-bit little-endian ELF (`ELFCLASS64`, `ELFDATA2LSB`).
pub fn section_bytes<'a>(file: &'a [u8], section_name: &str) -> Result<&'a [u8], ElfError> {
    if file.len() < 64 {
        return Err(ElfError { message: "ELF: file too small" });
    }
    if &file[0..4] != b"\x7FELF" {
        return Err(ElfError { message: "ELF: bad magic" });
    }
    if file[4] != 2 {
        return Err(ElfError { message: "ELF: only ELF64 is supported" });
    }
    if file[5] != 1 {
        return Err(ElfError { message: "ELF: only little-endian is supported" });
    }

    let e_shoff = u64_le(file, 0x28)? as usize;
    let e_shentsize = u16_le(file, 0x3A)? as usize;
    let e_shnum = u16_le(file, 0x3C)? as usize;
    let e_shstrndx = u16_le(file, 0x3E)? as usize;

    if e_shentsize == 0 {
        return Err(ElfError { message: "ELF: e_shentsize=0" });
    }
    if e_shnum == 0 {
        return Err(ElfError { message: "ELF: no section headers" });
    }
    if e_shstrndx >= e_shnum {
        return Err(ElfError { message: "ELF: e_shstrndx out of range" });
    }

    let sht = file
        .get(e_shoff..)
        .ok_or(ElfError { message: "ELF: section header table offset out of range" })?;
    let needed = e_shentsize
        .checked_mul(e_shnum)
        .ok_or(ElfError { message: "ELF: section header table size overflow" })?;
    if sht.len() < needed {
        return Err(ElfError { message: "ELF: truncated section header table" });
    }

    // Locate section header string table.
    let shstr_off = e_shoff + e_shstrndx * e_shentsize;
    let shstr = file
        .get(shstr_off..shstr_off + e_shentsize)
        .ok_or(ElfError { message: "ELF: shstrtab header out of range" })?;
    if shstr.len() < 64 {
        return Err(ElfError { message: "ELF: expected 64-byte section headers" });
    }
    let shstr_offset = u64_le(shstr, 24)? as usize;
    let shstr_size = u64_le(shstr, 32)? as usize;
    let shstr_bytes = file.get(shstr_offset..shstr_offset + shstr_size).ok_or(ElfError {
        message: "ELF: shstrtab section out of range",
    })?;

    for idx in 0..e_shnum {
        let off = e_shoff + idx * e_shentsize;
        let sh = file
            .get(off..off + e_shentsize)
            .ok_or(ElfError { message: "ELF: section header out of range" })?;
        if sh.len() < 64 {
            return Err(ElfError { message: "ELF: expected 64-byte section headers" });
        }

        let sh_name = u32_le(sh, 0)? as usize;
        let sh_offset = u64_le(sh, 24)? as usize;
        let sh_size = u64_le(sh, 32)? as usize;

        let name = get_cstr(shstr_bytes, sh_name)?;
        if name == section_name {
            return file.get(sh_offset..sh_offset + sh_size).ok_or(ElfError {
                message: "ELF: section range out of file bounds",
            });
        }
    }

    Err(ElfError { message: "ELF: section not found" })
}
