//! Loader for the current process's LLVM stackmaps section (`.llvm_stackmaps`).
//!
//! On Linux/ELF we support three strategies:
//! 1) **Fast path (zero I/O):** use linker-defined start/stop symbols emitted by
//!    `runtime-native/link/stackmaps.ld`.
//! 2) **Fallback (zero I/O):** scan mapped PT_LOAD segments via `dl_iterate_phdr`
//!    and look for StackMap v3 blobs.
//! 3) **Fallback (I/O):** parse `/proc/self/exe` to locate the section in the ELF
//!    image and then return a pointer into the already-loaded in-memory image.
//!
//! The fast path is optional because it requires a linker script. When the
//! symbols aren't available, we fall back to in-memory scanning, and as a last
//! resort ELF parsing so non-AOT tools/tests still work.
//!
//! On macOS/Mach-O, LLVM typically emits stackmaps into
//! `__LLVM_STACKMAPS,__llvm_stackmaps`; we use dyld APIs to locate that section.

use std::sync::OnceLock;

#[cfg(target_os = "linux")]
mod linux {
  use core::arch::global_asm;

  // Provide weak fallback definitions so `runtime-native` can link even when the final executable
  // does not define stackmap boundary symbols (e.g. tools/tests, or binaries without statepoints).
  //
  // We intentionally define *non-absolute* symbols (in `.bss`) so referencing them is valid even
  // when building `cdylib` artifacts (absolute symbols can trigger disallowed relocations).
  //
  // When `runtime-native/link/stackmaps.ld` defines the real range symbols, those strong
  // definitions override these weak fallbacks.
  global_asm!(
    r#"
    .pushsection .bss
    .balign 1

    // All weak fallbacks alias a single sentinel address so the runtime can
    // distinguish:
    // - symbols absent (start=end=sentinel) -> fall back to other discovery
    // - symbols present but empty (start=end!=sentinel) -> return empty slice
    .weak __runtime_native_stackmaps_fallback
    .weak __llvm_stackmaps_start
    .weak __llvm_stackmaps_end
    .weak __fastr_stackmaps_start
    .weak __fastr_stackmaps_end
    __runtime_native_stackmaps_fallback:
    __llvm_stackmaps_start:
    __llvm_stackmaps_end:
    __fastr_stackmaps_start:
    __fastr_stackmaps_end:
    .byte 0

    .popsection
    "#
  );

  extern "C" {
    pub static __runtime_native_stackmaps_fallback: u8;
    pub static __llvm_stackmaps_start: u8;
    pub static __llvm_stackmaps_end: u8;
    pub static __fastr_stackmaps_start: u8;
    pub static __fastr_stackmaps_end: u8;
  }
}

/// Output section names that may contain stackmap payloads in the final ELF.
///
/// Some link setups place stackmaps into `.data.rel.ro.llvm_stackmaps` so the
/// dynamic loader can relocate the section for PIE binaries (writable during
/// relocation, then protected by RELRO).
#[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
const LLVM_STACKMAPS_ELF_SECTION_CANDIDATES: [&str; 3] = [
  ".data.rel.ro.llvm_stackmaps",
  ".llvm_stackmaps",
  // Some linker scripts export an output section without the leading dot.
  "llvm_stackmaps",
];

// Cache the resolved stackmaps slice so we do any ELF parsing at most once.
static STACKMAPS_SECTION: OnceLock<&'static [u8]> = OnceLock::new();

/// Return the bytes of the current process's `.llvm_stackmaps` section.
///
/// - On Linux, this prefers linker-defined start/stop symbols when available,
///   and falls back to parsing `/proc/self/exe`.
/// - On macOS, we use `getsectdatafromheader_64`.
pub fn stackmaps_section() -> &'static [u8] {
  *STACKMAPS_SECTION.get_or_init(|| {
    if let Some(bytes) = load_llvm_stackmaps_via_symbols() {
      // Even if the section is empty (no stackmaps in this binary), treat the
      // linker-defined range as authoritative and avoid falling back to the
      // `/proc/self/exe` loader (which would do I/O just to discover the same).
      return bytes;
    }

    #[cfg(target_os = "linux")]
    if let Some(bytes) = try_load_stackmaps_from_self_linux_phdr() {
      return bytes;
    }

    #[cfg(target_os = "macos")]
    unsafe {
      let bytes = macho::stackmaps_section();
      if !bytes.is_empty() {
        return bytes;
      }
    }

    load_llvm_stackmaps_via_elf().unwrap_or(&[])
  })
}

/// Stable API name: load stack maps for the current binary.
pub fn load_stackmaps_from_self() -> &'static [u8] {
  stackmaps_section()
}

/// Attempt to load `.llvm_stackmaps` via linker-defined range symbols.
///
/// This is the preferred path on Linux: no `/proc` access and no ELF parsing.
pub fn try_load_via_linker_symbols() -> Option<&'static [u8]> {
  #[cfg(target_os = "linux")]
  unsafe {
    unsafe fn slice_from_range(start: *const u8, end: *const u8) -> Option<&'static [u8]> {
      let start_addr = start as usize;
      let end_addr = end as usize;
      if end_addr <= start_addr {
        return None;
      }

      let len = end_addr - start_addr;

      // StackMap v3 payload contains 64-bit fields and is 8-byte aligned.
      if start_addr % 8 != 0 || len % 8 != 0 {
        return None;
      }

      // Stack maps are metadata; if this is enormous something went very wrong (e.g. the symbols
      // resolved to unrelated addresses).
      const MAX_LEN: usize = 512 * 1024 * 1024; // 512 MiB
      if len > MAX_LEN {
        return None;
      }

      Some(core::slice::from_raw_parts(start, len))
    }

    let fallback = core::ptr::addr_of!(linux::__runtime_native_stackmaps_fallback) as usize;

    let try_pair = |start: *const u8, end: *const u8| -> Option<&'static [u8]> {
      let start_addr = start as usize;
      let end_addr = end as usize;

      // Symbols not provided by the final link (we're seeing our weak `.bss` fallbacks).
      if start_addr == fallback && end_addr == fallback {
        return None;
      }

      if end_addr < start_addr {
        return None;
      }

      // Symbols present but empty: treat as authoritative so we don't fall back to `/proc` I/O.
      if end_addr == start_addr {
        return Some(&[]);
      }

      slice_from_range(start, end)
    };

    try_pair(
      core::ptr::addr_of!(linux::__llvm_stackmaps_start),
      core::ptr::addr_of!(linux::__llvm_stackmaps_end),
    )
    .or_else(|| {
      try_pair(
        core::ptr::addr_of!(linux::__fastr_stackmaps_start),
        core::ptr::addr_of!(linux::__fastr_stackmaps_end),
      )
    })
  }

  #[cfg(not(target_os = "linux"))]
  None
}

/// Backwards-compatible alias for older callers.
pub fn load_llvm_stackmaps_via_symbols() -> Option<&'static [u8]> {
  try_load_via_linker_symbols()
}

fn load_llvm_stackmaps_via_elf() -> Option<&'static [u8]> {
  #[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
  {
    for name in LLVM_STACKMAPS_ELF_SECTION_CANDIDATES {
      if let Some(bytes) = load_elf64le_section_from_self(name) {
        return Some(bytes);
      }
    }
    return None;
  }

  #[cfg(not(all(target_os = "linux", target_pointer_width = "64", target_endian = "little")))]
  {
    None
  }
}

#[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
fn load_elf64le_section_from_self(section_name: &str) -> Option<&'static [u8]> {
  use std::ffi::CStr;
  use std::fs::File;
  use std::io::{Read, Seek, SeekFrom};

  let mut file = File::open("/proc/self/exe").ok()?;

  // ELF64 header is 64 bytes.
  let mut ehdr = [0u8; 0x40];
  file.read_exact(&mut ehdr).ok()?;

  if &ehdr[0..4] != b"\x7fELF" {
    return None;
  }
  // ELFCLASS64 + little-endian.
  if ehdr[4] != 2 || ehdr[5] != 1 {
    return None;
  }

  let e_shoff = u64::from_le_bytes(ehdr[0x28..0x30].try_into().ok()?);
  let e_shentsize = u16::from_le_bytes(ehdr[0x3A..0x3C].try_into().ok()?) as usize;
  let e_shnum = u16::from_le_bytes(ehdr[0x3C..0x3E].try_into().ok()?) as usize;
  let e_shstrndx = u16::from_le_bytes(ehdr[0x3E..0x40].try_into().ok()?) as usize;

  if e_shoff == 0 || e_shentsize == 0 || e_shnum == 0 || e_shstrndx >= e_shnum {
    return None;
  }

  let sh_table_size = e_shentsize.checked_mul(e_shnum)?;
  let mut sh_table = vec![0u8; sh_table_size];
  file.seek(SeekFrom::Start(e_shoff)).ok()?;
  file.read_exact(&mut sh_table).ok()?;

  let sh_at = |idx: usize| -> Option<&[u8]> {
    let start = idx.checked_mul(e_shentsize)?;
    let end = start.checked_add(e_shentsize)?;
    sh_table.get(start..end)
  };

  let shstr = sh_at(e_shstrndx)?;
  // We read `sh_offset` (0x18..0x20) and `sh_size` (0x20..0x28).
  if shstr.len() < 0x28 {
    return None;
  }

  let shstr_off = u64::from_le_bytes(shstr[0x18..0x20].try_into().ok()?);
  let shstr_size = u64::from_le_bytes(shstr[0x20..0x28].try_into().ok()?);
  let shstr_size_usize = usize::try_from(shstr_size).ok()?;

  let mut strtab = vec![0u8; shstr_size_usize];
  file.seek(SeekFrom::Start(shstr_off)).ok()?;
  file.read_exact(&mut strtab).ok()?;

  let mut target_addr: Option<u64> = None;
  let mut target_size: Option<u64> = None;

  for i in 0..e_shnum {
    let sh = sh_at(i)?;
    if sh.len() < 0x28 {
      return None;
    }

    let sh_name = u32::from_le_bytes(sh[0..4].try_into().ok()?) as usize;
    let name_bytes = strtab.get(sh_name..)?;
    let nul = name_bytes.iter().position(|&b| b == 0)?;
    let name = CStr::from_bytes_with_nul(name_bytes.get(..=nul)?).ok()?;
    if name.to_str().ok()? != section_name {
      continue;
    }

    let sh_addr = u64::from_le_bytes(sh[0x10..0x18].try_into().ok()?);
    let sh_size = u64::from_le_bytes(sh[0x20..0x28].try_into().ok()?);
    target_addr = Some(sh_addr);
    target_size = Some(sh_size);
    break;
  }

  let sh_addr = target_addr?;
  let sh_size = target_size?;
  let len = usize::try_from(sh_size).ok()?;
  if len == 0 {
    return None;
  }

  let (base, phdrs) = main_executable_phdrs()?;
  let start = base.checked_add(usize::try_from(sh_addr).ok()?)?;
  let end = start.checked_add(len)?;

  if !range_in_readable_load_segment(base, phdrs, start, end) {
    return None;
  }

  // Safety: we verified `start..end` lies within a readable PT_LOAD segment for the
  // main executable.
  Some(unsafe { std::slice::from_raw_parts(start as *const u8, len) })
}

#[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
fn main_executable_phdrs() -> Option<(usize, &'static [libc::Elf64_Phdr])> {
  use std::ffi::CStr;

  #[derive(Default)]
  struct Found {
    base: usize,
    phdr: *const libc::Elf64_Phdr,
    phnum: usize,
  }

  unsafe extern "C" fn cb(
    info: *mut libc::dl_phdr_info,
    _size: usize,
    data: *mut libc::c_void,
  ) -> libc::c_int {
    // Safety: called by the loader, `info` is valid for the duration of callback.
    let info = unsafe { &*info };

    let name_empty = if info.dlpi_name.is_null() {
      true
    } else {
      // Safety: dlpi_name is NUL-terminated.
      let s = unsafe { CStr::from_ptr(info.dlpi_name) };
      s.to_bytes().is_empty()
    };

    if !name_empty {
      return 0;
    }

    // Safety: `data` points at a `Found` for the duration of dl_iterate_phdr.
    let found = unsafe { &mut *(data as *mut Found) };
    found.base = info.dlpi_addr as usize;
    found.phdr = info.dlpi_phdr as *const libc::Elf64_Phdr;
    found.phnum = info.dlpi_phnum as usize;

    // Stop iteration.
    1
  }

  let mut found = Found::default();
  let rc = unsafe { libc::dl_iterate_phdr(Some(cb), (&mut found as *mut Found).cast()) };
  let _ = rc;

  if found.phdr.is_null() || found.phnum == 0 {
    return None;
  }

  // Safety: `dlpi_phdr` points to a valid program header array in the loaded image.
  let phdrs = unsafe { std::slice::from_raw_parts(found.phdr, found.phnum) };
  Some((found.base, phdrs))
}

#[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
fn range_in_readable_load_segment(
  base: usize,
  phdrs: &[libc::Elf64_Phdr],
  start: usize,
  end: usize,
) -> bool {
  for ph in phdrs {
    if ph.p_type != libc::PT_LOAD {
      continue;
    }
    if (ph.p_flags & libc::PF_R) == 0 {
      continue;
    }

    let seg_start = match base.checked_add(ph.p_vaddr as usize) {
      Some(v) => v,
      None => continue,
    };
    let seg_end = match seg_start.checked_add(ph.p_memsz as usize) {
      Some(v) => v,
      None => continue,
    };

    if seg_start <= start && end <= seg_end {
      return true;
    }
  }
  false
}

#[cfg(target_os = "macos")]
mod macho {
  use core::slice;

  // We only need an opaque handle for the Mach-O header pointer.
  #[repr(C)]
  pub struct MachHeader64 {
    _opaque: [u8; 0],
  }

  extern "C" {
    pub fn _dyld_get_image_header(image_index: u32) -> *const core::ffi::c_void;
    pub fn getsectdatafromheader_64(
      mh: *const MachHeader64,
      segname: *const libc::c_char,
      sectname: *const libc::c_char,
      size: *mut u64,
    ) -> *const u8;
  }

  /// Best-effort lookup of LLVM stackmaps in the main Mach-O image.
  ///
  /// LLVM typically emits stackmaps into segment/section:
  /// `__LLVM_STACKMAPS,__llvm_stackmaps`.
  pub unsafe fn stackmaps_section() -> &'static [u8] {
    let header = _dyld_get_image_header(0);
    if header.is_null() {
      return &[];
    }

    let mut size: u64 = 0;
    let ptr = getsectdatafromheader_64(
      header.cast::<MachHeader64>(),
      b"__LLVM_STACKMAPS\0".as_ptr().cast(),
      b"__llvm_stackmaps\0".as_ptr().cast(),
      &mut size,
    );

    if ptr.is_null() || size == 0 {
      return &[];
    }

    let len = match usize::try_from(size) {
      Ok(len) => len,
      Err(_) => {
        panic!(".llvm_stackmaps size overflow on macOS: size={size}");
      }
    };

    // StackMap v3 payload contains 64-bit fields and is 8-byte aligned.
    if (ptr as usize) % 8 != 0 {
      panic!(
        ".llvm_stackmaps pointer misaligned on macOS: ptr={:#x}",
        ptr as usize
      );
    }
    if len % 8 != 0 {
      panic!(".llvm_stackmaps length misaligned on macOS: len={len}");
    }

    slice::from_raw_parts(ptr, len)
  }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
  use super::stackmaps_section;
  use crate::StackMaps;

  #[repr(align(8))]
  struct Aligned<const N: usize>([u8; N]);

  // Minimal StackMap v3 header with zero functions/constants/records.
  #[used]
  #[link_section = "__LLVM_STACKMAPS,__llvm_stackmaps"]
  static TEST_STACKMAP_BLOB: Aligned<16> = Aligned([
    3, 0, 0, 0, // version + reserved
    0, 0, 0, 0, // num_functions
    0, 0, 0, 0, // num_constants
    0, 0, 0, 0, // num_records
  ]);

  #[test]
  fn discovers_macho_stackmaps_section_and_parses() {
    let section = stackmaps_section();
    assert!(
      section.len() >= TEST_STACKMAP_BLOB.0.len(),
      "expected .llvm_stackmaps section to be present"
    );
    assert_eq!(&section[..16], &TEST_STACKMAP_BLOB.0);

    let parsed = StackMaps::parse(section).expect("stackmaps should parse");
    assert_eq!(parsed.raw().version, 3);
    assert_eq!(parsed.raw().records.len(), 0);
  }
}

/// Best-effort lookup of the loaded stackmaps section for the current Linux
/// process by scanning mapped PT_LOAD segments via `dl_iterate_phdr`.
///
/// This is a zero-I/O fallback when the binary is not linked with a stackmaps
/// linker script.
pub fn try_load_stackmaps_from_self_linux_phdr() -> Option<&'static [u8]> {
  #[cfg(target_os = "linux")]
  {
    static CACHE: OnceLock<Option<&'static [u8]>> = OnceLock::new();
    return *CACHE.get_or_init(|| unsafe { linux_phdr::scan_for_stackmaps() });
  }

  #[cfg(not(target_os = "linux"))]
  None
}

#[cfg(target_os = "linux")]
mod linux_phdr {
  use core::ffi::c_void;
  use core::slice;

  use libc::{c_int, dl_iterate_phdr, dl_phdr_info, size_t};

  use crate::stackmaps::{StackMap, STACKMAP_VERSION};

  #[cfg(target_pointer_width = "64")]
  type ElfPhdr = libc::Elf64_Phdr;
  #[cfg(target_pointer_width = "32")]
  type ElfPhdr = libc::Elf32_Phdr;

  const STACKMAP_V3_HEADER: [u8; 4] = [STACKMAP_VERSION, 0, 0, 0];

  // Hard safety/perf caps (this path is best-effort and must not accidentally
  // scan unbounded memory ranges if the process has unusual mappings).
  const MAX_TOTAL_SCAN_BYTES: usize = 512 * 1024 * 1024; // 512 MiB
  const MAX_SEGMENT_SCAN_BYTES: usize = 256 * 1024 * 1024; // 256 MiB
  const MAX_SECTION_BYTES: usize = 512 * 1024 * 1024; // 512 MiB
  const MAX_CANDIDATE_ATTEMPTS: usize = 128;
  const MAX_CONCAT_BLOBS: usize = 4096;

  #[derive(Clone, Copy)]
  struct Candidate {
    ptr: *const u8,
    len: usize,
    score: u32,
  }

  struct ScanState {
    best: Option<Candidate>,
    total_scanned: usize,
    candidate_attempts: usize,
  }

  pub unsafe fn scan_for_stackmaps() -> Option<&'static [u8]> {
    let mut state = ScanState {
      best: None,
      total_scanned: 0,
      candidate_attempts: 0,
    };

    // Safety: we only pass a pointer to our stack-allocated `state` and keep it
    // alive for the duration of the call.
    dl_iterate_phdr(Some(callback), (&mut state as *mut ScanState).cast::<c_void>());

    state
      .best
      .map(|c| unsafe { slice::from_raw_parts(c.ptr, c.len) })
  }

  unsafe extern "C" fn callback(
    info: *mut dl_phdr_info,
    _size: size_t,
    data: *mut c_void,
  ) -> c_int {
    let state = unsafe { &mut *data.cast::<ScanState>() };
    if state.total_scanned >= MAX_TOTAL_SCAN_BYTES || state.candidate_attempts >= MAX_CANDIDATE_ATTEMPTS {
      return 1;
    }

    let Some(info) = (unsafe { info.as_ref() }) else {
      return 0;
    };

    let base = info.dlpi_addr as u64;
    let is_main = unsafe {
      let name = info.dlpi_name;
      name.is_null() || *name == 0
    };

    let phnum = info.dlpi_phnum as usize;
    let phdrs = unsafe { slice::from_raw_parts(info.dlpi_phdr.cast::<ElfPhdr>(), phnum) };

    let mut exec_ranges: Vec<core::ops::Range<u64>> = Vec::new();
    for ph in phdrs {
      if ph.p_type != libc::PT_LOAD {
        continue;
      }
      if (ph.p_flags & libc::PF_X) == 0 {
        continue;
      }

      let Some(start) = base.checked_add(ph.p_vaddr as u64) else {
        continue;
      };
      let Some(end) = start.checked_add(ph.p_memsz as u64) else {
        continue;
      };
      exec_ranges.push(start..end);
    }

    // Prefer non-executable segments (stackmaps are metadata).
    for want_exec in [false, true] {
      for ph in phdrs {
        if ph.p_type != libc::PT_LOAD {
          continue;
        }
        if (ph.p_flags & libc::PF_R) == 0 {
          continue;
        }

        let seg_is_exec = (ph.p_flags & libc::PF_X) != 0;
        if seg_is_exec != want_exec {
          continue;
        }

        let Some(seg_start) = base.checked_add(ph.p_vaddr as u64) else {
          continue;
        };

        let filesz = ph.p_filesz as u64;
        if filesz == 0 {
          continue;
        }

        let scan_len = core::cmp::min(filesz, MAX_SEGMENT_SCAN_BYTES as u64);
        let Ok(scan_len) = usize::try_from(scan_len) else {
          continue;
        };
        if scan_len == 0 {
          continue;
        }

        let budget = MAX_TOTAL_SCAN_BYTES.saturating_sub(state.total_scanned);
        let scan_len = core::cmp::min(scan_len, budget);
        if scan_len == 0 {
          return 1;
        }

        let Ok(seg_start_usize) = usize::try_from(seg_start) else {
          continue;
        };

        let seg = unsafe { slice::from_raw_parts(seg_start_usize as *const u8, scan_len) };
        state.total_scanned = state.total_scanned.saturating_add(scan_len);

        if let Some((ptr, len)) =
          scan_segment_for_stackmaps(seg_start, ph.p_flags as u32, seg, &exec_ranges, state)
        {
          let score = score_candidate(is_main, ph.p_flags as u32);
          let cand = Candidate { ptr, len, score };
          if is_better(cand, state.best) {
            state.best = Some(cand);
          }

          // Main executable + non-exec segment is the highest confidence match we can get; stop early.
          if is_main && (ph.p_flags & libc::PF_X) == 0 {
            return 1;
          }
        }

        if state.candidate_attempts >= MAX_CANDIDATE_ATTEMPTS {
          return 1;
        }
      }
    }

    0
  }

  fn score_candidate(is_main: bool, flags: u32) -> u32 {
    let mut score = 0;
    if is_main {
      score += 4;
    }
    if (flags & libc::PF_X as u32) == 0 {
      score += 2;
    }
    score
  }

  fn is_better(new: Candidate, cur: Option<Candidate>) -> bool {
    let Some(cur) = cur else {
      return true;
    };
    if new.score != cur.score {
      return new.score > cur.score;
    }
    new.len > cur.len
  }

  fn scan_segment_for_stackmaps(
    seg_start: u64,
    seg_flags: u32,
    seg: &[u8],
    exec_ranges: &[core::ops::Range<u64>],
    state: &mut ScanState,
  ) -> Option<(*const u8, usize)> {
    // StackMap v3 payload contains 64-bit fields and is 8-byte aligned.
    let Ok(seg_start_usize) = usize::try_from(seg_start) else {
      return None;
    };
    let mut off = (8 - (seg_start_usize % 8)) % 8;

    let mut best: Option<(*const u8, usize)> = None;

    while off + 16 <= seg.len() {
      if state.candidate_attempts >= MAX_CANDIDATE_ATTEMPTS {
        break;
      }

      if seg.get(off..off + 4) != Some(&STACKMAP_V3_HEADER) {
        off += 8;
        continue;
      }

      state.candidate_attempts += 1;

      let remaining = &seg[off..];
      let cap = core::cmp::min(remaining.len(), MAX_SECTION_BYTES);
      let bytes = &remaining[..cap];

      let Some(len) = try_parse_stackmaps_region(bytes, exec_ranges) else {
        off += 8;
        continue;
      };

      // If we had to truncate the segment tail for MAX_SECTION_BYTES and the region consumes all
      // available bytes, reject instead of potentially returning a partial section.
      if remaining.len() > MAX_SECTION_BYTES && len == bytes.len() {
        off += 8;
        continue;
      }

      // Prefer non-exec segments when multiple candidates exist; score is applied by caller.
      let start_ptr = unsafe { (seg_start_usize as *const u8).add(off) };

      // StackMap v3 payload contains 64-bit fields and is 8-byte aligned.
      if (start_ptr as usize) % 8 != 0 || len % 8 != 0 {
        off += 8;
        continue;
      }

      // Don't accept an empty region.
      if len == 0 {
        off += 8;
        continue;
      }

      // `seg_flags` used to gate scanning order, but keep it in the signature so it's harder to
      // accidentally call this helper on non-PT_LOAD data in the future.
      let _ = seg_flags;

      match best {
        Some((_best_ptr, best_len)) if best_len >= len => {}
        _ => best = Some((start_ptr, len)),
      }

      // Continue scanning; multiple matches are possible (false positives or multiple images of the
      // same blob in memory).
      off += 8;
      continue;
    }

    best
  }

  fn try_parse_stackmaps_region(
    bytes: &[u8],
    exec_ranges: &[core::ops::Range<u64>],
  ) -> Option<usize> {
    let mut off: usize = 0;
    let mut end: usize = 0;
    let mut parsed_any = false;
    let mut blobs = 0usize;

    while off < bytes.len() {
      // Skip linker padding between concatenated blobs.
      while off < bytes.len() && bytes[off] == 0 {
        off += 1;
      }
      if off >= bytes.len() {
        break;
      }

      // Only accept a blob start when the v3 header signature matches. If it doesn't, we've likely
      // reached the next unrelated section in the same PT_LOAD segment.
      if bytes.get(off..off + 4) != Some(&STACKMAP_V3_HEADER) {
        break;
      }

      if blobs >= MAX_CONCAT_BLOBS {
        return None;
      }

      let parse_res = StackMap::parse_with_len(&bytes[off..]);
      let (map, len) = match parse_res {
        Ok(v) => v,
        Err(_) => {
          // Candidate start must parse; later failures are treated as end-of-section heuristics.
          if parsed_any {
            break;
          }
          return None;
        }
      };

      // Quick sanity checks to avoid false positives and pathological allocations.
      if len == 0 || len % 8 != 0 {
        if parsed_any {
          break;
        }
        return None;
      }

      if map.functions.is_empty() || map.records.is_empty() {
        if parsed_any {
          break;
        }
        return None;
      }

      // Verify that function addresses refer to executable memory in the same image. This is a
      // strong filter against accidental matches inside unrelated data.
      for func in &map.functions {
        if !exec_ranges.iter().any(|r| r.contains(&func.address)) {
          if parsed_any {
            break;
          }
          return None;
        }
      }

      // Basic internal consistency: the sum of per-function record_count must match the record table.
      let mut expected_records: u64 = 0;
      for func in &map.functions {
        expected_records = expected_records.checked_add(func.record_count)?;
      }
      if expected_records != map.records.len() as u64 {
        if parsed_any {
          break;
        }
        return None;
      }

      parsed_any = true;
      blobs += 1;
      off = off.checked_add(len)?;
      end = off;

      if end > MAX_SECTION_BYTES {
        return None;
      }
    }

    parsed_any.then_some(end)
  }
}
