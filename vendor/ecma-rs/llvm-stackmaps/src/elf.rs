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
    let shstr_off = e_shstrndx
        .checked_mul(e_shentsize)
        .and_then(|delta| e_shoff.checked_add(delta))
        .ok_or(ElfError { message: "ELF: shstrtab header offset overflow" })?;
    let shstr_end = shstr_off
        .checked_add(e_shentsize)
        .ok_or(ElfError { message: "ELF: shstrtab header end offset overflow" })?;
    let shstr = file
        .get(shstr_off..shstr_end)
        .ok_or(ElfError { message: "ELF: shstrtab header out of range" })?;
    if shstr.len() < 64 {
        return Err(ElfError { message: "ELF: expected 64-byte section headers" });
    }
    let shstr_offset = u64_le(shstr, 24)? as usize;
    let shstr_size = u64_le(shstr, 32)? as usize;
    let shstr_end = shstr_offset
        .checked_add(shstr_size)
        .ok_or(ElfError { message: "ELF: shstrtab size overflow" })?;
    let shstr_bytes = file.get(shstr_offset..shstr_end).ok_or(ElfError {
        message: "ELF: shstrtab section out of range",
    })?;

    for idx in 0..e_shnum {
        let off = idx
            .checked_mul(e_shentsize)
            .and_then(|delta| e_shoff.checked_add(delta))
            .ok_or(ElfError { message: "ELF: section header offset overflow" })?;
        let sh_end = off
            .checked_add(e_shentsize)
            .ok_or(ElfError { message: "ELF: section header end offset overflow" })?;
        let sh = file
            .get(off..sh_end)
            .ok_or(ElfError { message: "ELF: section header out of range" })?;
        if sh.len() < 64 {
            return Err(ElfError { message: "ELF: expected 64-byte section headers" });
        }

        let sh_name = u32_le(sh, 0)? as usize;
        let sh_offset = u64_le(sh, 24)? as usize;
        let sh_size = u64_le(sh, 32)? as usize;

        let name = get_cstr(shstr_bytes, sh_name)?;
        if name == section_name {
            let end = sh_offset
                .checked_add(sh_size)
                .ok_or(ElfError { message: "ELF: section size overflow" })?;
            return file.get(sh_offset..end).ok_or(ElfError {
                message: "ELF: section range out of file bounds",
            });
        }
    }

    Err(ElfError { message: "ELF: section not found" })
}

/// Return the raw stackmaps section bytes from an ELF file.
///
/// In object files, LLVM writes stackmaps into `.llvm_stackmaps`.
/// In final PIE binaries, this repository's link pipeline relocates them into
/// `.data.rel.ro.llvm_stackmaps` so runtime relocations can be applied safely.
pub fn stackmaps_section_bytes<'a>(file: &'a [u8]) -> Result<&'a [u8], ElfError> {
    section_bytes(file, ".data.rel.ro.llvm_stackmaps")
        .or_else(|_| section_bytes(file, ".llvm_stackmaps"))
        // Some link pipelines may rename the output section to drop the leading dot so GNU ld/lld
        // can auto-synthesize `__start_`/`__stop_` symbols. This repo's default uses explicit symbol
        // definitions in `stackmaps.ld`, but accept this name for tooling compatibility.
        .or_else(|_| section_bytes(file, "llvm_stackmaps"))
}

#[cfg(test)]
mod tests {
    use super::section_bytes;

    #[test]
    fn section_bytes_rejects_shstrtab_size_overflow() {
        let mut file = vec![0u8; 128];

        file[0..4].copy_from_slice(b"\x7FELF");
        file[4] = 2; // ELFCLASS64
        file[5] = 1; // ELFDATA2LSB

        file[0x28..0x30].copy_from_slice(&(64u64).to_le_bytes()); // e_shoff
        file[0x3A..0x3C].copy_from_slice(&(64u16).to_le_bytes()); // e_shentsize
        file[0x3C..0x3E].copy_from_slice(&(1u16).to_le_bytes()); // e_shnum
        file[0x3E..0x40].copy_from_slice(&(0u16).to_le_bytes()); // e_shstrndx

        let shstr = &mut file[64..128];
        shstr[24..32].copy_from_slice(&u64::MAX.to_le_bytes()); // sh_offset
        shstr[32..40].copy_from_slice(&(1u64).to_le_bytes()); // sh_size

        let err = section_bytes(&file, ".llvm_stackmaps").unwrap_err();
        assert_eq!(err.message, "ELF: shstrtab size overflow");
    }
}
