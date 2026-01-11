use object::{Object, ObjectSection, ObjectSymbol};
use std::ops::Range;

/// ELF section names that may contain LLVM stackmaps.
///
/// `.data.rel.ro.llvm_stackmaps` is preferred because it avoids DT_TEXTREL for PIE/DSO builds.
const STACKMAP_SECTION_NAMES: [&str; 3] = [
  ".data.rel.ro.llvm_stackmaps",
  ".llvm_stackmaps",
  // Linker-script exported output section from Task 288.
  "llvm_stackmaps",
];

/// Start/stop symbols emitted by linker scripts.
///
/// Prefer symbol-based discovery when present because section headers may be stripped.
///
/// - `__fastr_stackmaps_*` / `__llvm_stackmaps_*` are used by `runtime-native/stackmaps.ld`
/// - `__start_llvm_stackmaps` / `__stop_llvm_stackmaps` are a GNU ld / lld convention for a
///   linker-script exported `llvm_stackmaps` output section (Task 288).
const STACKMAP_SYMBOL_RANGES: [(&str, &str); 3] = [
  ("__fastr_stackmaps_start", "__fastr_stackmaps_end"),
  ("__llvm_stackmaps_start", "__llvm_stackmaps_end"),
  ("__start_llvm_stackmaps", "__stop_llvm_stackmaps"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackMapSectionSource {
  LinkerSymbols,
  SectionName(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackMapSection<'a> {
  pub source: StackMapSectionSource,
  pub bytes: &'a [u8],
}

#[derive(Debug, thiserror::Error)]
pub enum StackMapLoadError {
  #[error("failed to parse object file: {0}")]
  Object(#[from] object::Error),

  #[error("stackmap section not found")]
  NotFound,

  #[error("malformed LLVM stackmap section: {0}")]
  Parse(#[from] StackMapParseError),
}

#[derive(Debug, thiserror::Error)]
pub enum StackMapParseError {
  #[error("unexpected end of data at offset {0}")]
  UnexpectedEof(usize),

  #[error("unsupported LLVM StackMap version {0}")]
  UnsupportedVersion(u8),

  #[error("trailing non-zero bytes after parsing stackmaps at offset {0}")]
  TrailingNonZero(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackMapBlob {
  pub version: u8,
  pub num_functions: u32,
  pub num_constants: u32,
  pub num_records: u32,
  pub record_ids: Vec<u64>,
}

/// Find the stackmap data in an ELF/Mach-O/COFF file.
///
/// Discovery order:
/// 1. Linker-script start/stop symbols (`__start_llvm_stackmaps` / `__stop_llvm_stackmaps`)
/// 2. Section name lookup for known section names
pub fn find_stackmap_section<'a>(
  file_bytes: &'a [u8],
) -> Result<Option<StackMapSection<'a>>, StackMapLoadError> {
  let obj = object::File::parse(file_bytes)?;

  for (start_sym, stop_sym) in STACKMAP_SYMBOL_RANGES {
    if let Some(range) = find_range_via_start_stop_symbols(&obj, start_sym, stop_sym)? {
      let start = range.start as usize;
      let end = range.end as usize;
      return Ok(Some(StackMapSection {
        source: StackMapSectionSource::LinkerSymbols,
        bytes: &file_bytes[start..end],
      }));
    }
  }

  for name in STACKMAP_SECTION_NAMES {
    if let Some(section) = obj.section_by_name(name) {
      let bytes = section.data()?;
      return Ok(Some(StackMapSection {
        source: StackMapSectionSource::SectionName(name),
        bytes,
      }));
    }
  }

  Ok(None)
}

pub fn load_and_parse_stackmaps(file_bytes: &[u8]) -> Result<Vec<StackMapBlob>, StackMapLoadError> {
  let section = find_stackmap_section(file_bytes)?.ok_or(StackMapLoadError::NotFound)?;
  parse_stackmap_blobs(section.bytes).map_err(Into::into)
}

/// Parse one or more concatenated LLVM stackmap "blobs".
///
/// When linking multiple objects, the linker concatenates their `.llvm_stackmaps` sections, so
/// the output section is commonly a sequence of independent stackmap blobs.
pub fn parse_stackmap_blobs(section_bytes: &[u8]) -> Result<Vec<StackMapBlob>, StackMapParseError> {
  let mut offset = 0usize;
  let mut blobs = Vec::new();

  while offset < section_bytes.len() {
    // Allow padding between blobs (linker alignment) and at end.
    while offset < section_bytes.len() && section_bytes[offset] == 0 {
      offset += 1;
    }
    if offset >= section_bytes.len() {
      break;
    }

    let (blob, consumed) = parse_stackmap_blob(&section_bytes[offset..])?;
    blobs.push(blob);
    offset += consumed;
  }

  if offset < section_bytes.len() && section_bytes[offset..].iter().any(|&b| b != 0) {
    return Err(StackMapParseError::TrailingNonZero(offset));
  }

  Ok(blobs)
}

fn parse_stackmap_blob(data: &[u8]) -> Result<(StackMapBlob, usize), StackMapParseError> {
  let mut c = Cursor::new(data);

  let version = c.read_u8()?;
  c.read_u8()?; // reserved0
  c.read_u16()?; // reserved1

  // LLVM currently emits version 3 (LLVM 18).
  if version != 3 {
    return Err(StackMapParseError::UnsupportedVersion(version));
  }

  let num_functions = c.read_u32()?;
  let num_constants = c.read_u32()?;
  let num_records = c.read_u32()?;

  // Function records (24 bytes each)
  for _ in 0..num_functions {
    c.read_u64()?; // function address
    c.read_u64()?; // stack size
    c.read_u64()?; // record count
  }

  // Constants (u64 each)
  for _ in 0..num_constants {
    c.read_u64()?;
  }

  let mut record_ids = Vec::with_capacity(num_records as usize);
  for _ in 0..num_records {
    let record_id = c.read_u64()?;
    record_ids.push(record_id);

    c.read_u32()?; // instruction offset
    c.read_u16()?; // reserved
    let num_locations = c.read_u16()?;

    for _ in 0..num_locations {
      c.read_u8()?; // kind
      c.read_u8()?; // reserved
      c.read_u16()?; // size
      c.read_u16()?; // dwarf reg num
      c.read_u16()?; // reserved
      c.read_i32()?; // offset / constant
    }

    c.read_u16()?; // padding (align live-outs)
    let num_live_outs = c.read_u16()?;
    for _ in 0..num_live_outs {
      c.read_u16()?; // dwarf reg num
      c.read_u8()?; // reserved
      c.read_u8()?; // size
    }

    // Trailing padding: LLVM writes a u32, then pads to 8-byte alignment (usually another u32).
    c.read_u32()?;
    if c.offset() % 8 != 0 {
      c.read_u32()?;
    }
  }

  Ok((
    StackMapBlob {
      version,
      num_functions,
      num_constants,
      num_records,
      record_ids,
    },
    c.offset(),
  ))
}

fn find_range_via_start_stop_symbols(
  obj: &object::File<'_>,
  start_sym: &str,
  stop_sym: &str,
) -> Result<Option<Range<u64>>, object::Error> {
  let Some((start_addr, start_sec)) = find_symbol_addr_and_section(obj, start_sym)?
  else {
    return Ok(None);
  };
  let Some((stop_addr, stop_sec)) = find_symbol_addr_and_section(obj, stop_sym)? else {
    return Ok(None);
  };

  if start_sec != stop_sec {
    return Ok(None);
  }

  let section = obj.section_by_index(start_sec)?;
  let Some((section_file_off, _section_size)) = section.file_range() else {
    return Ok(None);
  };

  let section_addr = section.address();
  let Some(start_delta) = start_addr.checked_sub(section_addr) else {
    return Ok(None);
  };
  let Some(stop_delta) = stop_addr.checked_sub(section_addr) else {
    return Ok(None);
  };
  let start_off = section_file_off + start_delta;
  let stop_off = section_file_off + stop_delta;
  if stop_off < start_off {
    return Ok(None);
  }
  Ok(Some(start_off..stop_off))
}

fn find_symbol_addr_and_section(
  obj: &object::File<'_>,
  name: &str,
) -> Result<Option<(u64, object::SectionIndex)>, object::Error> {
  for sym in obj.symbols().chain(obj.dynamic_symbols()) {
    if sym.name()? != name {
      continue;
    }
    if let Some(sec) = sym.section_index() {
      return Ok(Some((sym.address(), sec)));
    }
  }
  Ok(None)
}

struct Cursor<'a> {
  data: &'a [u8],
  offset: usize,
}

impl<'a> Cursor<'a> {
  fn new(data: &'a [u8]) -> Self {
    Self { data, offset: 0 }
  }

  fn offset(&self) -> usize {
    self.offset
  }

  fn read_u8(&mut self) -> Result<u8, StackMapParseError> {
    let Some(b) = self.data.get(self.offset).copied() else {
      return Err(StackMapParseError::UnexpectedEof(self.offset));
    };
    self.offset += 1;
    Ok(b)
  }

  fn read_u16(&mut self) -> Result<u16, StackMapParseError> {
    let bytes = self.take::<2>()?;
    Ok(u16::from_le_bytes(bytes))
  }

  fn read_u32(&mut self) -> Result<u32, StackMapParseError> {
    let bytes = self.take::<4>()?;
    Ok(u32::from_le_bytes(bytes))
  }

  fn read_u64(&mut self) -> Result<u64, StackMapParseError> {
    let bytes = self.take::<8>()?;
    Ok(u64::from_le_bytes(bytes))
  }

  fn read_i32(&mut self) -> Result<i32, StackMapParseError> {
    let bytes = self.take::<4>()?;
    Ok(i32::from_le_bytes(bytes))
  }

  fn take<const N: usize>(&mut self) -> Result<[u8; N], StackMapParseError> {
    let end = self
      .offset
      .checked_add(N)
      .ok_or(StackMapParseError::UnexpectedEof(self.offset))?;
    let Some(slice) = self.data.get(self.offset..end) else {
      return Err(StackMapParseError::UnexpectedEof(self.offset));
    };
    self.offset = end;
    Ok(slice.try_into().expect("slice length already checked"))
  }
}
