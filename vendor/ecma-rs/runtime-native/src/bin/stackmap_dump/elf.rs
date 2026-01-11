use crate::endian::Endian;
use anyhow::{bail, Context, Result};

pub struct ElfSection<'a> {
  pub endian: Endian,
  pub data: &'a [u8],
}

fn read_bytes<'a>(data: &'a [u8], offset: usize, len: usize) -> Result<&'a [u8]> {
  data.get(offset..offset + len).with_context(|| {
    format!(
      "out of bounds read: offset={offset} len={len} file_len={}",
      data.len()
    )
  })
}

fn read_array<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N]> {
  let bytes = read_bytes(data, offset, N)?;
  Ok(bytes.try_into().expect("slice length checked"))
}

/// Extract an ELF section by name.
///
/// Notes:
/// - ELF32 and ELF64 are supported.
/// - Extended section numbering (`e_shnum == 0`/`SHN_XINDEX`) is not supported.
pub fn extract_section<'a>(file: &'a [u8], section_name: &str) -> Result<ElfSection<'a>> {
  const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
  if file.get(0..4) != Some(ELF_MAGIC) {
    bail!("not an ELF file (missing magic header)");
  }

  let class = *file.get(4).context("missing ELF class")?;
  let endian = match *file.get(5).context("missing ELF endianness")? {
    1 => Endian::Little,
    2 => Endian::Big,
    other => bail!("unknown ELF endianness: {other}"),
  };

  match class {
    1 => extract_section_elf32(file, endian, section_name),
    2 => extract_section_elf64(file, endian, section_name),
    other => bail!("unknown ELF class: {other}"),
  }
}

fn extract_section_elf32<'a>(
  file: &'a [u8],
  endian: Endian,
  section_name: &str,
) -> Result<ElfSection<'a>> {
  // Offsets in ELF32 header.
  let e_shoff = endian.read_u32(read_array(file, 32)?) as usize;
  let e_shentsize = endian.read_u16(read_array(file, 46)?) as usize;
  let e_shnum = endian.read_u16(read_array(file, 48)?) as usize;
  let e_shstrndx = endian.read_u16(read_array(file, 50)?) as usize;

  if e_shnum == 0 {
    bail!("unsupported ELF: extended section numbering (e_shnum == 0)");
  }
  if e_shstrndx >= e_shnum {
    bail!("invalid ELF: e_shstrndx ({e_shstrndx}) >= e_shnum ({e_shnum})");
  }

  let shstr = read_section_header_elf32(file, endian, e_shoff, e_shentsize, e_shstrndx)?;
  let shstrtab = read_bytes(file, shstr.sh_offset, shstr.sh_size)?;

  for idx in 0..e_shnum {
    let sh = read_section_header_elf32(file, endian, e_shoff, e_shentsize, idx)?;
    let name = read_cstr(shstrtab, sh.sh_name)?;
    if name == section_name {
      let data = read_bytes(file, sh.sh_offset, sh.sh_size)?;
      return Ok(ElfSection { endian, data });
    }
  }

  bail!("ELF section not found: {section_name}");
}

fn extract_section_elf64<'a>(
  file: &'a [u8],
  endian: Endian,
  section_name: &str,
) -> Result<ElfSection<'a>> {
  // Offsets in ELF64 header.
  let e_shoff = endian.read_u64(read_array(file, 40)?) as usize;
  let e_shentsize = endian.read_u16(read_array(file, 58)?) as usize;
  let e_shnum = endian.read_u16(read_array(file, 60)?) as usize;
  let e_shstrndx = endian.read_u16(read_array(file, 62)?) as usize;

  if e_shnum == 0 {
    bail!("unsupported ELF: extended section numbering (e_shnum == 0)");
  }
  if e_shstrndx >= e_shnum {
    bail!("invalid ELF: e_shstrndx ({e_shstrndx}) >= e_shnum ({e_shnum})");
  }

  let shstr = read_section_header_elf64(file, endian, e_shoff, e_shentsize, e_shstrndx)?;
  let shstrtab = read_bytes(file, shstr.sh_offset, shstr.sh_size)?;

  for idx in 0..e_shnum {
    let sh = read_section_header_elf64(file, endian, e_shoff, e_shentsize, idx)?;
    let name = read_cstr(shstrtab, sh.sh_name)?;
    if name == section_name {
      let data = read_bytes(file, sh.sh_offset, sh.sh_size)?;
      return Ok(ElfSection { endian, data });
    }
  }

  bail!("ELF section not found: {section_name}");
}

fn read_cstr(data: &[u8], offset: u32) -> Result<&str> {
  let offset = offset as usize;
  let rest = data
    .get(offset..)
    .with_context(|| format!("ELF string offset out of bounds: {offset}"))?;
  let end = rest
    .iter()
    .position(|&b| b == 0)
    .context("ELF string not NUL-terminated")?;
  std::str::from_utf8(&rest[..end]).context("ELF string is not UTF-8")
}

struct SectionHeaderElf32 {
  sh_name: u32,
  sh_offset: usize,
  sh_size: usize,
}

fn read_section_header_elf32(
  file: &[u8],
  endian: Endian,
  e_shoff: usize,
  e_shentsize: usize,
  idx: usize,
) -> Result<SectionHeaderElf32> {
  // ELF32 section header offsets.
  // sh_name: 0x00 u32
  // sh_offset: 0x10 u32
  // sh_size: 0x14 u32
  let base = e_shoff
    .checked_add(
      e_shentsize
        .checked_mul(idx)
        .context("section header offset overflow")?,
    )
    .context("section header offset overflow")?;

  let sh_name = endian.read_u32(read_array(file, base + 0x00)?);
  let sh_offset = endian.read_u32(read_array(file, base + 0x10)?) as usize;
  let sh_size = endian.read_u32(read_array(file, base + 0x14)?) as usize;
  Ok(SectionHeaderElf32 {
    sh_name,
    sh_offset,
    sh_size,
  })
}

struct SectionHeaderElf64 {
  sh_name: u32,
  sh_offset: usize,
  sh_size: usize,
}

fn read_section_header_elf64(
  file: &[u8],
  endian: Endian,
  e_shoff: usize,
  e_shentsize: usize,
  idx: usize,
) -> Result<SectionHeaderElf64> {
  // ELF64 section header offsets.
  // sh_name: 0x00 u32
  // sh_offset: 0x18 u64
  // sh_size: 0x20 u64
  let base = e_shoff
    .checked_add(
      e_shentsize
        .checked_mul(idx)
        .context("section header offset overflow")?,
    )
    .context("section header offset overflow")?;

  let sh_name = endian.read_u32(read_array(file, base + 0x00)?);
  let sh_offset = endian.read_u64(read_array(file, base + 0x18)?) as usize;
  let sh_size = endian.read_u64(read_array(file, base + 0x20)?) as usize;
  Ok(SectionHeaderElf64 {
    sh_name,
    sh_offset,
    sh_size,
  })
}

