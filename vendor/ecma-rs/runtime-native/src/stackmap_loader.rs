use crate::stackmaps::{CallSite, StackMaps};
use anyhow::Context;
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
/// - `__start_llvm_stackmaps` / `__stop_llvm_stackmaps` are stable boundary symbols defined by the
///   linker-script fragments in:
///   - `runtime-native/link/stackmaps.ld` (lld-friendly)
///   - `runtime-native/link/stackmaps_gnuld.ld` (GNU ld PIE hardening)
/// - `__stackmaps_{start,end}` is a generic alias used by `llvm-stackmaps` and other tooling.
/// - `__fastr_stackmaps_*` / `__llvm_stackmaps_*` are legacy/project-specific aliases.
const STACKMAP_SYMBOL_RANGES: [(&str, &str); 4] = [
  ("__start_llvm_stackmaps", "__stop_llvm_stackmaps"),
  ("__stackmaps_start", "__stackmaps_end"),
  ("__fastr_stackmaps_start", "__fastr_stackmaps_end"),
  ("__llvm_stackmaps_start", "__llvm_stackmaps_end"),
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
  const STACKMAP_V3_HEADER_SIZE: usize = 16;
  const STACKMAP_VERSION: u8 = 3;

  let mut offset = 0usize;
  let mut blobs = Vec::new();

  while offset < section_bytes.len() {
    // Allow padding between blobs (linker alignment) and at end.
    while offset < section_bytes.len() && section_bytes[offset] == 0 {
      offset += 1;
    }
    if offset >= section_bytes.len() || section_bytes.len() - offset < STACKMAP_V3_HEADER_SIZE {
      break;
    }

    if section_bytes[offset] != STACKMAP_VERSION {
      // Some toolchains have been observed to leave short non-zero padding bytes between
      // concatenated `.llvm_stackmaps` input sections. Try to recover by scanning forward for the
      // next plausible v3 header (version=3, reserved bytes=0).
      const MAX_PADDING_SCAN: usize = 256;
      let scan_end = (offset + MAX_PADDING_SCAN)
        .min(section_bytes.len().saturating_sub(STACKMAP_V3_HEADER_SIZE));
 
      let mut found: Option<usize> = None;
      for i in offset + 1..=scan_end {
        if section_bytes[i] == STACKMAP_VERSION
          && section_bytes[i + 1] == 0
          && section_bytes[i + 2] == 0
          && section_bytes[i + 3] == 0
        {
          found = Some(i);
          break;
        }
      }
 
      if let Some(i) = found {
        offset = i;
        continue;
      }
 
      return Err(StackMapParseError::TrailingNonZero(offset));
    }

    let (blob, consumed) = parse_stackmap_blob(&section_bytes[offset..])?;
    blobs.push(blob);
    if consumed == 0 {
      return Err(StackMapParseError::UnexpectedEof(offset));
    }
    offset = offset
      .checked_add(consumed)
      .ok_or(StackMapParseError::UnexpectedEof(offset))?;
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

  // Each record is at least 24 bytes, even with 0 locations and 0 live-outs.
  // Validate the record count against the remaining bytes to avoid huge
  // allocations on malformed inputs (e.g. `num_records = u32::MAX` in a short
  // buffer).
  const MIN_RECORD_SIZE: usize = 24;
  let remaining = data.len().saturating_sub(c.offset());
  if num_records as usize > remaining / MIN_RECORD_SIZE {
    return Err(StackMapParseError::UnexpectedEof(c.offset()));
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

    // StackMap v3 aligns the live-out header to 8 bytes after the locations array.
    c.align_to(8)?;
    c.read_u16()?; // padding
    let num_live_outs = c.read_u16()?;
    for _ in 0..num_live_outs {
      c.read_u16()?; // dwarf reg num
      c.read_u8()?; // reserved
      c.read_u8()?; // size
    }

    // Records are 8-byte aligned after the live-out array.
    c.align_to(8)?;
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
  let Some((start_addr, start_sec)) = find_symbol_addr_and_section(obj, start_sym)? else {
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

  fn align_to(&mut self, align: usize) -> Result<(), StackMapParseError> {
    debug_assert!(align.is_power_of_two());
    let add = self
      .offset
      .checked_add(align - 1)
      .ok_or(StackMapParseError::UnexpectedEof(self.offset))?;
    let new_offset = add & !(align - 1);
    if new_offset > self.data.len() {
      return Err(StackMapParseError::UnexpectedEof(self.offset));
    }
    self.offset = new_offset;
    Ok(())
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

/// Load the current process's in-memory `.llvm_stackmaps` section.
///
/// This is a convenience wrapper for consumers that only care about stackmaps in
/// the main executable. For environments that can `dlopen` managed code, prefer
/// [`load_all_llvm_stackmaps`] + [`build_global_stackmap_index`].
pub fn load_llvm_stackmaps() -> anyhow::Result<&'static [u8]> {
  #[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
  {
    use std::ffi::CStr;
    use std::os::raw::c_int;

    let file = std::fs::read("/proc/self/exe").context("read /proc/self/exe for stackmap discovery")?;
    let elf = object::File::parse(&*file).context("parse /proc/self/exe as ELF")?;

    let section = find_stackmap_section_vaddr_and_size(&elf)?
      .ok_or_else(|| anyhow::anyhow!("stackmap section not found in /proc/self/exe"))?;
    if section.size == 0 {
      anyhow::bail!("stackmap section exists but is empty");
    }

    // Stack maps are metadata; if this is enormous something went very wrong
    // (e.g. we mis-parsed the ELF or are looking at the wrong section).
    const MAX_STACKMAP_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB
    if section.size > MAX_STACKMAP_BYTES {
      anyhow::bail!(
        "invalid stackmaps section size: {size} bytes (max {MAX_STACKMAP_BYTES})",
        size = section.size
      );
    }

    struct Ctx {
      vaddr: u64,
      size: u64,
      out: Option<&'static [u8]>,
      err: Option<anyhow::Error>,
    }

    unsafe extern "C" fn cb(
      info: *mut libc::dl_phdr_info,
      _size: libc::size_t,
      data: *mut libc::c_void,
    ) -> c_int {
      let ctx = &mut *(data as *mut Ctx);
      if ctx.err.is_some() {
        return 1;
      }

      let info = &*info;
      let base: u64 = info.dlpi_addr as u64;
      let name_bytes = if info.dlpi_name.is_null() {
        &b""[..]
      } else {
        CStr::from_ptr(info.dlpi_name).to_bytes()
      };
      // glibc reports the main executable with an empty name.
      if !name_bytes.is_empty() {
        return 0;
      }

      let Some(start) = base.checked_add(ctx.vaddr) else {
        ctx.err = Some(anyhow::anyhow!(
          "address overflow computing stackmaps start: base={base:#x} vaddr={vaddr:#x}",
          vaddr = ctx.vaddr
        ));
        return 1;
      };
      let Some(end) = start.checked_add(ctx.size) else {
        ctx.err = Some(anyhow::anyhow!(
          "address overflow computing stackmaps end: start={start:#x} size={size:#x}",
          size = ctx.size
        ));
        return 1;
      };

      // Ensure the computed range is within a readable PT_LOAD segment.
      if !range_in_readable_load_segment(info, base, start, end) {
        ctx.err = Some(anyhow::anyhow!(
          "stackmaps section range [{start:#x},{end:#x}) is not covered by a readable PT_LOAD segment"
        ));
        return 1;
      }

      let len = match usize::try_from(ctx.size) {
        Ok(l) => l,
        Err(_) => {
          ctx.err = Some(anyhow::anyhow!(
            "stackmaps section size does not fit usize: {size}",
            size = ctx.size
          ));
          return 1;
        }
      };

      // Safety: stackmaps section is expected to be mapped as a readable segment
      // for the lifetime of the executable.
      ctx.out = Some(std::slice::from_raw_parts(start as *const u8, len));
      1
    }

    let mut ctx = Ctx {
      vaddr: section.vaddr,
      size: section.size,
      out: None,
      err: None,
    };
    unsafe {
      libc::dl_iterate_phdr(Some(cb), (&mut ctx as *mut Ctx).cast());
    }

    if let Some(err) = ctx.err {
      return Err(err);
    }
    ctx
      .out
      .ok_or_else(|| anyhow::anyhow!("dl_iterate_phdr did not report the main executable"))
  }

  #[cfg(not(all(target_os = "linux", target_pointer_width = "64", target_endian = "little")))]
  {
    anyhow::bail!("load_llvm_stackmaps is only supported on Linux 64-bit little-endian targets");
  }
}

/// Collect all in-memory `.llvm_stackmaps` sections across all loaded ELF images
/// (main executable + DSOs).
///
/// This is primarily intended for environments where compiled code (and thus
/// stackmaps) can live in shared libraries loaded via `dlopen`.
///
/// ## Refreshing after `dlopen`
/// `dl_iterate_phdr` enumerates **currently loaded** images. Call this again
/// after `dlopen` if you need stackmaps from newly loaded DSOs.
pub fn load_all_llvm_stackmaps() -> anyhow::Result<Vec<&'static [u8]>> {
  #[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
  {
    use std::ffi::{CStr, OsStr};
    use std::os::raw::c_int;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    struct Ctx {
      out: Vec<&'static [u8]>,
      err: Option<anyhow::Error>,
    }

    unsafe extern "C" fn cb(
      info: *mut libc::dl_phdr_info,
      _size: libc::size_t,
      data: *mut libc::c_void,
    ) -> c_int {
      let ctx = &mut *(data as *mut Ctx);
      if ctx.err.is_some() {
        return 1;
      }

      let info = &*info;

      let base: u64 = info.dlpi_addr as u64;
      let name_bytes = if info.dlpi_name.is_null() {
        &b""[..]
      } else {
        CStr::from_ptr(info.dlpi_name).to_bytes()
      };

      let is_main_exe = name_bytes.is_empty();
      let path: &Path = if is_main_exe {
        Path::new("/proc/self/exe")
      } else {
        Path::new(OsStr::from_bytes(name_bytes))
      };

      let file = match std::fs::read(path) {
        Ok(b) => b,
        Err(err) => {
          // Some images (e.g. linux-vdso) do not have an on-disk file.
          if is_main_exe {
            ctx.err = Some(anyhow::anyhow!(
              "failed to read {} for stackmap discovery: {err}",
              path.display()
            ));
            return 1;
          }
          return 0;
        }
      };

      let elf = match object::File::parse(&*file) {
        Ok(o) => o,
        Err(err) => {
          if is_main_exe {
            ctx.err = Some(anyhow::Error::new(err).context("parse main executable"));
            return 1;
          }
          return 0;
        }
      };

      let section = match find_stackmap_section_vaddr_and_size(&elf) {
        Ok(Some(v)) => v,
        Ok(None) => return 0,
        Err(err) => {
          if is_main_exe {
            ctx.err = Some(anyhow::Error::new(err).context("locate .llvm_stackmaps in main executable"));
            return 1;
          }
          return 0;
        }
      };

      if section.size == 0 {
        return 0;
      }

      let Some(start) = base.checked_add(section.vaddr) else {
        if is_main_exe {
          ctx.err = Some(anyhow::anyhow!(
            "address overflow computing stackmaps start: base={base:#x} vaddr={vaddr:#x}",
            vaddr = section.vaddr
          ));
          return 1;
        }
        return 0;
      };
      let Some(end) = start.checked_add(section.size) else {
        if is_main_exe {
          ctx.err = Some(anyhow::anyhow!(
            "address overflow computing stackmaps end: start={start:#x} size={size:#x}",
            size = section.size
          ));
          return 1;
        }
        return 0;
      };

      // Stack maps are metadata; if this is enormous something went very wrong
      // (e.g. we mis-parsed the ELF or are looking at the wrong section).
      const MAX_STACKMAP_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB
      if section.size > MAX_STACKMAP_BYTES {
        if is_main_exe {
          ctx.err = Some(anyhow::anyhow!(
            "invalid stackmaps section size: {size} bytes (max {MAX_STACKMAP_BYTES})",
            size = section.size
          ));
          return 1;
        }
        return 0;
      }

      // Ensure the computed range is within a readable PT_LOAD segment.
      if !range_in_readable_load_segment(info, base, start, end) {
        if is_main_exe {
          ctx.err = Some(anyhow::anyhow!(
            "stackmaps section range [{start:#x},{end:#x}) is not covered by a readable PT_LOAD segment"
          ));
          return 1;
        }
        return 0;
      }

      let len = match usize::try_from(section.size) {
        Ok(l) => l,
        Err(_) => {
          if is_main_exe {
            ctx.err = Some(anyhow::anyhow!(
              "stackmaps section size does not fit usize: {size}",
              size = section.size
            ));
            return 1;
          }
          return 0;
        }
      };

      // Safety: stackmaps section is expected to be mapped as a readable segment
      // for the lifetime of the loaded image.
      let bytes: &'static [u8] = std::slice::from_raw_parts(start as *const u8, len);
      ctx.out.push(bytes);

      0
    }

    let mut ctx = Ctx {
      out: Vec::new(),
      err: None,
    };

    unsafe {
      libc::dl_iterate_phdr(Some(cb), (&mut ctx as *mut Ctx).cast());
    }

    if let Some(err) = ctx.err {
      return Err(err);
    }

    Ok(ctx.out)
  }

  #[cfg(not(all(target_os = "linux", target_pointer_width = "64", target_endian = "little")))]
  {
    anyhow::bail!("load_all_llvm_stackmaps is only supported on Linux 64-bit little-endian");
  }
}

/// Global stackmap index keyed by absolute callsite PC (return address).
///
/// This is a merged view across all stackmap blobs discovered via
/// [`load_all_llvm_stackmaps`].
#[derive(Debug)]
pub struct StackMapIndex {
  blobs: Vec<StackMaps>,
  callsites: Vec<GlobalCallsiteEntry>,
}

#[derive(Debug, Clone, Copy)]
struct GlobalCallsiteEntry {
  pc: u64,
  stack_size: u64,
  blob_index: usize,
  stackmap_index: usize,
  record_index: usize,
}

impl StackMapIndex {
  fn new(blobs: Vec<StackMaps>) -> anyhow::Result<Self> {
    let mut callsites: Vec<GlobalCallsiteEntry> = Vec::new();
    for (blob_index, blob) in blobs.iter().enumerate() {
      for entry in blob.callsites() {
        callsites.push(GlobalCallsiteEntry {
          pc: entry.pc,
          stack_size: entry.stack_size,
          blob_index,
          stackmap_index: entry.stackmap_index,
          record_index: entry.record_index,
        });
      }
    }

    callsites.sort_by_key(|e| e.pc);

    for win in callsites.windows(2) {
      let [a, b] = win else { continue };
      if a.pc == b.pc {
        anyhow::bail!("duplicate stackmap callsite pc found: {pc:#x}", pc = a.pc);
      }
    }

    Ok(Self { blobs, callsites })
  }

  pub fn is_empty(&self) -> bool {
    self.callsites.is_empty()
  }

  pub fn len(&self) -> usize {
    self.callsites.len()
  }

  pub fn lookup(&self, callsite_return_addr: u64) -> Option<CallSite<'_>> {
    let idx = self
      .callsites
      .binary_search_by_key(&callsite_return_addr, |e| e.pc)
      .ok()?;
    let entry = &self.callsites[idx];
    let blob = &self.blobs[entry.blob_index];
    let raw = blob.raws().get(entry.stackmap_index)?;
    Some(CallSite {
      stack_size: entry.stack_size,
      record: raw.records.get(entry.record_index)?,
    })
  }

  pub fn iter(&self) -> impl Iterator<Item = (u64, CallSite<'_>)> + '_ {
    self.callsites.iter().map(|entry| {
      let blob = &self.blobs[entry.blob_index];
      let raw = &blob.raws()[entry.stackmap_index];
      (
        entry.pc,
        CallSite {
          stack_size: entry.stack_size,
          record: &raw.records[entry.record_index],
        },
      )
    })
  }
}

/// Build a unified [`StackMapIndex`] from all stackmap sections found in
/// currently loaded ELF images.
///
/// Call this again after `dlopen` if you need to refresh the index with newly
/// loaded DSOs.
pub fn build_global_stackmap_index() -> anyhow::Result<StackMapIndex> {
  let sections = load_all_llvm_stackmaps()?;

  let mut blobs: Vec<StackMaps> = Vec::with_capacity(sections.len());
  for section in sections {
    blobs.push(StackMaps::parse(section).with_context(|| "failed to parse stackmaps section")?);
  }

  StackMapIndex::new(blobs)
}

#[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
#[derive(Debug, Clone, Copy)]
struct ElfSectionVaddr {
  vaddr: u64,
  size: u64,
}

#[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
fn find_stackmap_section_vaddr_and_size(
  obj: &object::File<'_>,
) -> Result<Option<ElfSectionVaddr>, object::Error> {
  for name in STACKMAP_SECTION_NAMES {
    if let Some(section) = obj.section_by_name(name) {
      return Ok(Some(ElfSectionVaddr {
        vaddr: section.address(),
        size: section.size(),
      }));
    }
  }

  for (start_sym, stop_sym) in STACKMAP_SYMBOL_RANGES {
    let Some((start_addr, start_sec)) = find_symbol_addr_and_section(obj, start_sym)? else {
      continue;
    };
    let Some((stop_addr, stop_sec)) = find_symbol_addr_and_section(obj, stop_sym)? else {
      continue;
    };
    if start_sec != stop_sec {
      continue;
    }
    let Some(size) = stop_addr.checked_sub(start_addr) else {
      continue;
    };
    return Ok(Some(ElfSectionVaddr {
      vaddr: start_addr,
      size,
    }));
  }

  Ok(None)
}

#[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
fn range_in_readable_load_segment(
  info: &libc::dl_phdr_info,
  base: u64,
  start: u64,
  end: u64,
) -> bool {
  const PT_LOAD: u32 = 1;
  const PF_R: u32 = 4;

  if info.dlpi_phdr.is_null() {
    return false;
  }

  let phnum = info.dlpi_phnum as usize;
  let phdrs = info.dlpi_phdr as *const libc::Elf64_Phdr;
  for i in 0..phnum {
    // Safety: `dlpi_phdr` points to an array of `dlpi_phnum` program headers.
    let ph = unsafe { &*phdrs.add(i) };
    if ph.p_type != PT_LOAD || (ph.p_flags & PF_R) == 0 {
      continue;
    }

    let Some(seg_start) = base.checked_add(ph.p_vaddr) else {
      continue;
    };
    let Some(seg_end) = seg_start.checked_add(ph.p_memsz) else {
      continue;
    };
    if seg_start <= start && end <= seg_end {
      return true;
    }
  }

  false
}

#[cfg(test)]
mod tests {
  use super::{parse_stackmap_blobs, StackMapBlob};

  fn push_u8(buf: &mut Vec<u8>, v: u8) {
    buf.push(v);
  }
  fn push_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
  }
  fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
  }
  fn push_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
  }
  fn push_i32(buf: &mut Vec<u8>, v: i32) {
    buf.extend_from_slice(&v.to_le_bytes());
  }

  fn align_to_8(buf: &mut Vec<u8>) {
    while buf.len() % 8 != 0 {
      buf.push(0);
    }
  }

  fn build_blob(record_id: u64, num_locations: u16, num_live_outs: u16) -> Vec<u8> {
    let mut b = Vec::new();

    // Header.
    push_u8(&mut b, 3);
    push_u8(&mut b, 0);
    push_u16(&mut b, 0);
    push_u32(&mut b, 1); // num_functions
    push_u32(&mut b, 0); // num_constants
    push_u32(&mut b, 1); // num_records

    // Function record.
    push_u64(&mut b, 0);
    push_u64(&mut b, 0);
    push_u64(&mut b, 1);

    // Record.
    push_u64(&mut b, record_id);
    push_u32(&mut b, 0);
    push_u16(&mut b, 0);
    push_u16(&mut b, num_locations);

    // Locations (12 bytes each). Kind=Register (1) with dummy fields.
    for i in 0..num_locations {
      let _ = i;
      push_u8(&mut b, 1);
      push_u8(&mut b, 0);
      push_u16(&mut b, 8);
      push_u16(&mut b, 0);
      push_u16(&mut b, 0);
      push_i32(&mut b, 0);
    }

    // Align to 8 before live-out header.
    align_to_8(&mut b);
    push_u16(&mut b, 0);
    push_u16(&mut b, num_live_outs);

    // LiveOut entries (4 bytes each).
    for _ in 0..num_live_outs {
      push_u16(&mut b, 0);
      push_u8(&mut b, 0);
      push_u8(&mut b, 8);
    }

    // Align record end to 8 (for next record / blob end).
    align_to_8(&mut b);
    b
  }

  #[test]
  fn parses_blob_with_live_outs_and_no_trailing_padding() {
    // Regression test: the loader must handle records where the live-out array leaves the
    // record already 8-byte aligned (so there is no trailing padding). This occurs when the
    // number of live-outs is odd and the locations array is already 8-byte aligned.
    //
    // Old parser logic always consumed at least one u32 of "trailing padding", which could
    // incorrectly read into the next blob or hit EOF.
    let blob = build_blob(0xAABBCCDD, 2, 1);
    let blobs = parse_stackmap_blobs(&blob).expect("parse stackmap blob");
    assert_eq!(blobs.len(), 1);
    assert_eq!(
      blobs[0],
      StackMapBlob {
        version: 3,
        num_functions: 1,
        num_constants: 0,
        num_records: 1,
        record_ids: vec![0xAABBCCDD],
      }
    );
  }

  #[test]
  fn parses_concatenated_blobs() {
    let mut bytes = Vec::new();
    bytes.extend(build_blob(1, 1, 0));
    bytes.extend([0u8; 8]);
    bytes.extend(build_blob(2, 2, 1));

    let blobs = parse_stackmap_blobs(&bytes).expect("parse concatenated blobs");
    let ids: Vec<u64> = blobs.iter().flat_map(|b| b.record_ids.iter().copied()).collect();
    assert_eq!(ids, vec![1, 2]);
  }

  #[test]
  fn ignores_short_trailing_non_zero_bytes() {
    let mut bytes = Vec::new();
    bytes.extend(build_blob(1, 1, 0));
    // Some toolchains can leave short (<16B) non-zero padding at the end of the section; ignore it.
    bytes.extend([0xAAu8; 8]);
 
    let blobs = parse_stackmap_blobs(&bytes).expect("parse stackmap blobs");
    assert_eq!(blobs.len(), 1);
    assert_eq!(blobs[0].record_ids, vec![1]);
  }
 
  #[test]
  fn skips_short_non_zero_padding_between_blobs() {
    let mut bytes = Vec::new();
    bytes.extend(build_blob(1, 1, 0));
    bytes.extend([0xAAu8; 8]);
    bytes.extend(build_blob(2, 1, 0));
 
    let blobs = parse_stackmap_blobs(&bytes).expect("parse stackmap blobs");
    let ids: Vec<u64> = blobs.iter().flat_map(|b| b.record_ids.iter().copied()).collect();
    assert_eq!(ids, vec![1, 2]);
  }
 
  #[test]
  fn rejects_overlarge_num_records_without_allocating() {
    // Header claims an absurd number of records but provides no record table. This should error
    // deterministically without attempting to allocate `record_ids` for `u32::MAX` entries.
    let mut bytes = Vec::new();
    push_u8(&mut bytes, 3); // version
    push_u8(&mut bytes, 0); // reserved0
    push_u16(&mut bytes, 0); // reserved1
    push_u32(&mut bytes, 0); // num_functions
    push_u32(&mut bytes, 0); // num_constants
    push_u32(&mut bytes, u32::MAX); // num_records

    let err = parse_stackmap_blobs(&bytes).unwrap_err();
    match err {
      super::StackMapParseError::UnexpectedEof(off) => assert_eq!(off, 16),
      other => panic!("expected UnexpectedEof(16), got {other:?}"),
    }
  }
}
