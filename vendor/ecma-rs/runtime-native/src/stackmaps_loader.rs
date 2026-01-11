//! Loader for the current process's LLVM stackmaps section (`.llvm_stackmaps`).
//!
//! On Linux/ELF we support two strategies:
//! 1) **Fast path (zero I/O):** use linker-defined start/stop symbols emitted by
//!    `runtime-native/link/stackmaps.ld`.
//! 2) **Fallback (I/O):** parse `/proc/self/exe` to locate the section in the ELF
//!    image and then return a pointer into the already-loaded in-memory image.
//!
//! The fast path is optional because it requires a linker script. When the
//! symbols aren't available, we fall back to ELF parsing so non-AOT tools/tests
//! still work.
//!
//! On macOS/Mach-O, LLVM typically emits stackmaps into
//! `__LLVM_STACKMAPS,__llvm_stackmaps`; we use dyld APIs to locate that section.

use std::sync::OnceLock;

/// Output section names that may contain stackmap payloads in the final ELF.
///
/// Some link setups place stackmaps into `.data.rel.ro.llvm_stackmaps` so the
/// dynamic loader can relocate the section for PIE binaries (writable during
/// relocation, then protected by RELRO).
#[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
const LLVM_STACKMAPS_ELF_SECTION_CANDIDATES: [&str; 2] =
  [".data.rel.ro.llvm_stackmaps", ".llvm_stackmaps"];

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

/// Attempt to load `.llvm_stackmaps` via linker-defined start/stop symbols.
///
/// This is the preferred path on Linux: no `/proc` access and no ELF parsing.
pub fn load_llvm_stackmaps_via_symbols() -> Option<&'static [u8]> {
  #[cfg(all(target_os = "linux", feature = "llvm_stackmaps_linker"))]
  unsafe {
    const LLVM_STACKMAPS_SECTION: &str = ".llvm_stackmaps";

    extern "C" {
      static __start_llvm_stackmaps: u8;
      static __stop_llvm_stackmaps: u8;
    }

    let start = core::ptr::addr_of!(__start_llvm_stackmaps) as usize;
    let end = core::ptr::addr_of!(__stop_llvm_stackmaps) as usize;

    if end < start {
      panic!(
        "invalid {LLVM_STACKMAPS_SECTION} range: __stop_llvm_stackmaps ({end:#x}) < __start_llvm_stackmaps ({start:#x})"
      );
    }

    let len = end - start;

    // StackMap v3 payload contains 64-bit fields and is 8-byte aligned.
    if start % 8 != 0 {
      panic!(
        "{LLVM_STACKMAPS_SECTION} pointer misaligned: __start_llvm_stackmaps={start:#x}"
      );
    }
    if len % 8 != 0 {
      panic!("{LLVM_STACKMAPS_SECTION} length misaligned: len={len}");
    }

    // Stack maps are metadata; if this is enormous something went very wrong
    // (e.g. the linker script wasn't applied and symbols resolved to unrelated
    // addresses).
    const MAX_LEN: usize = 512 * 1024 * 1024; // 512 MiB
    if len > MAX_LEN {
      panic!(
        "invalid {LLVM_STACKMAPS_SECTION} length: {len} bytes (max {MAX_LEN}); linker script probably not applied"
      );
    }

    Some(core::slice::from_raw_parts(start as *const u8, len))
  }

  #[cfg(not(all(target_os = "linux", feature = "llvm_stackmaps_linker")))]
  {
    None
  }
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
