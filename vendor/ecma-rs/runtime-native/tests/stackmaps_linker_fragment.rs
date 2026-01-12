#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol, SymbolScope};
use runtime_native::test_util::TestRuntimeGuard;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn write_file(path: &Path, contents: &str) {
  fs::write(path, contents).unwrap();
}

fn have_cmd(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

fn find_clang() -> Option<&'static str> {
  for cand in ["clang-18", "clang"] {
    if have_cmd(cand) {
      return Some(cand);
    }
  }
  None
}

fn lld_flag() -> Option<&'static str> {
  if have_cmd("ld.lld-18") {
    Some("-fuse-ld=lld-18")
  } else if have_cmd("ld.lld") {
    Some("-fuse-ld=lld")
  } else {
    None
  }
}

fn compile_obj_pie(clang: &str, out_dir: &Path) -> PathBuf {
  // Intentionally avoid emitting any `.rodata` or `.data` so the linker script
  // fragments can't rely on them existing. lld errors if an `INSERT` anchor
  // output section does not exist.
  let asm = r#"
 .text
 .globl f
 f:
   ret

 .section .data.rel.ro.llvm_stackmaps,"aw",@progbits
   .byte 1,2,3,4

 .section .note.GNU-stack,"",@progbits
  "#;

  let asm_path = out_dir.join("stackmaps.S");
  write_file(&asm_path, asm);

  let obj_path = out_dir.join("stackmaps.o");
  let status = Command::new(clang)
    .args(["-c", "-o"])
    .arg(&obj_path)
    .arg(&asm_path)
    .status()
    .unwrap();
  assert!(status.success());
  obj_path
}

fn compile_obj_nopie(clang: &str, out_dir: &Path) -> PathBuf {
  // Intentionally avoid emitting any `.rodata` or `.data` so the non-PIE linker
  // script fragment can't rely on them existing and so stackmaps don't end up
  // sharing a writable load segment with `.data` in tiny test binaries.
  let asm = r#"
 .text
 .globl f
 f:
   ret

 .section .llvm_stackmaps,"a",@progbits
   .byte 1,2,3,4

 .section .note.GNU-stack,"",@progbits
  "#;

  let asm_path = out_dir.join("stackmaps.S");
  write_file(&asm_path, asm);

  let obj_path = out_dir.join("stackmaps.o");
  let status = Command::new(clang)
    .args(["-c", "-o"])
    .arg(&obj_path)
    .arg(&asm_path)
    .status()
    .unwrap();
  assert!(status.success());
  obj_path
}

fn find_symbol<'data>(file: &object::File<'data>, name: &str) -> Option<(u64, SymbolScope)> {
  for sym in file.symbols() {
    if sym.name().ok() == Some(name) {
      return Some((sym.address(), sym.scope()));
    }
  }
  for sym in file.dynamic_symbols() {
    if sym.name().ok() == Some(name) {
      return Some((sym.address(), sym.scope()));
    }
  }
  None
}

fn segment_is_readable(flags: object::SegmentFlags) -> bool {
  // PF_R on ELF is bit 2 (value 4).
  match flags {
    object::SegmentFlags::Elf { p_flags } => (p_flags & 4) != 0,
    _ => true,
  }
}

fn segment_is_writable(flags: object::SegmentFlags) -> bool {
  // PF_W on ELF is bit 1 (value 2).
  match flags {
    object::SegmentFlags::Elf { p_flags } => (p_flags & 2) != 0,
    _ => false,
  }
}

#[test]
fn stackmaps_ld_fragment_links_without_rodata_and_exports_symbols() {
  let _rt = TestRuntimeGuard::new();
  let Some(lld_flag) = lld_flag() else {
    eprintln!("skipping: lld not found in PATH (need ld.lld-18 or ld.lld)");
    return;
  };
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang not found in PATH (need clang-18 or clang)");
    return;
  };

  const START_SYM: &str = "__start_llvm_stackmaps";
  const STOP_SYM: &str = "__stop_llvm_stackmaps";
  // Generic alias.
  const GENERIC_START_SYM: &str = "__stackmaps_start";
  const GENERIC_END_SYM: &str = "__stackmaps_end";
  // Legacy aliases (kept for compatibility with older tooling).
  const LEGACY_START_SYM: &str = "__llvm_stackmaps_start";
  const LEGACY_END_SYM: &str = "__llvm_stackmaps_end";
  const LEGACY_FASTR_START_SYM: &str = "__fastr_stackmaps_start";
  const LEGACY_FASTR_END_SYM: &str = "__fastr_stackmaps_end";

  let td = tempfile::tempdir().unwrap();
  let obj = compile_obj_pie(clang, td.path());

  let exe = td.path().join("a.out");
  let script = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("link")
    .join("stackmaps.ld");

  // Link a minimal PIE executable (no stdlib/CRT) using our linker script
  // fragment.
  let status = Command::new(clang)
    .arg("-nostdlib")
    .arg(lld_flag)
    .arg("-pie")
    // We never run the binary; we only inspect its sections/symbols.
    .arg("-Wl,-e,f")
    // Ensure stackmaps are still retained under dead-section elimination. The
    // linker script fragment uses `KEEP(*(.data.rel.ro.llvm_stackmaps ...))` to
    // prevent GC from discarding the section even if it's otherwise unreferenced.
    .arg("-Wl,--gc-sections")
    .arg(format!("-Wl,-T,{}", script.display()))
    .arg("-o")
    .arg(&exe)
    .arg(&obj)
    .status()
    .unwrap();
  assert!(status.success(), "linking failed");

  let bytes = fs::read(&exe).unwrap();
  let file = object::File::parse(&*bytes).unwrap();

  // The lld-oriented stackmaps linker fragment (`link/stackmaps.ld`) appends stackmaps into the
  // standard `.data.rel.ro` output section (RELRO-friendly). Some other link scripts/toolchains may
  // place the bytes into a dedicated `.data.rel.ro.llvm_stackmaps` output section, so accept
  // either.
  let section = file
    .section_by_name(".data.rel.ro.llvm_stackmaps")
    .or_else(|| file.section_by_name(".data.rel.ro"))
    .expect("missing stackmaps output section (.data.rel.ro.llvm_stackmaps or .data.rel.ro)");

  let section_addr = section.address();
  let section_size = section.size();
  assert!(section_size > 0, "expected non-empty stackmaps output section");

  let (start, start_scope) = find_symbol(&file, START_SYM).expect("missing __start_llvm_stackmaps");
  let (stop, stop_scope) = find_symbol(&file, STOP_SYM).expect("missing __stop_llvm_stackmaps");
  let (generic_start, generic_start_scope) =
    find_symbol(&file, GENERIC_START_SYM).expect("missing __stackmaps_start");
  let (generic_end, generic_end_scope) = find_symbol(&file, GENERIC_END_SYM).expect("missing __stackmaps_end");
  let (legacy_start, legacy_start_scope) =
    find_symbol(&file, LEGACY_START_SYM).expect("missing __llvm_stackmaps_start");
  let (legacy_end, legacy_end_scope) =
    find_symbol(&file, LEGACY_END_SYM).expect("missing __llvm_stackmaps_end");
  let (fastr_start, fastr_start_scope) =
    find_symbol(&file, LEGACY_FASTR_START_SYM).expect("missing __fastr_stackmaps_start");
  let (fastr_end, fastr_end_scope) =
    find_symbol(&file, LEGACY_FASTR_END_SYM).expect("missing __fastr_stackmaps_end");

  assert_ne!(
    start_scope,
    SymbolScope::Compilation,
    "{START_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    stop_scope,
    SymbolScope::Compilation,
    "{STOP_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    generic_start_scope,
    SymbolScope::Compilation,
    "{GENERIC_START_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    generic_end_scope,
    SymbolScope::Compilation,
    "{GENERIC_END_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    legacy_start_scope,
    SymbolScope::Compilation,
    "{LEGACY_START_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    legacy_end_scope,
    SymbolScope::Compilation,
    "{LEGACY_END_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    fastr_start_scope,
    SymbolScope::Compilation,
    "{LEGACY_FASTR_START_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    fastr_end_scope,
    SymbolScope::Compilation,
    "{LEGACY_FASTR_END_SYM} must be globally linkable (not a local symbol)"
  );

  assert!(
    stop > start,
    "invalid stackmaps symbol range: start=0x{start:x} stop=0x{stop:x}"
  );
  assert!(
    section_addr <= start && stop <= section_addr + section_size,
    "stackmaps symbol range must be contained in the stackmaps output section (section_addr=0x{section_addr:x} size=0x{section_size:x} start=0x{start:x} stop=0x{stop:x})"
  );

  assert_eq!(generic_start, start, "generic start symbol must match");
  assert_eq!(generic_end, stop, "generic end symbol must match");
  assert_eq!(legacy_start, start, "legacy start symbol must match");
  assert_eq!(legacy_end, stop, "legacy end symbol must match");
  assert_eq!(fastr_start, start, "fastr start symbol must match");
  assert_eq!(fastr_end, stop, "fastr end symbol must match");

  // Ensure the linker script actually retained our bytes (not just the symbols).
  let data = section.data().expect("read stackmaps output section bytes");
  let off = usize::try_from(start - section_addr).expect("offset fits usize");
  let len = usize::try_from(stop - start).expect("len fits usize");
  assert!(off + len <= data.len(), "stackmaps range out of bounds");
  assert!(
    data[off..].starts_with(&[1, 2, 3, 4]),
    "expected stackmaps payload to be preserved"
  );

  // Ensure the section is backed by a readable load segment so the runtime can
  // read the bytes directly from memory.
  let mut in_readable_segment = false;
  let section_end = stop;
  for seg in file.segments() {
    let seg_addr = seg.address();
    let seg_end = seg_addr + seg.size();
    let flags = seg.flags();
    if seg_addr <= start && section_end <= seg_end && segment_is_readable(flags) {
      in_readable_segment = true;
      break;
    }
  }
  assert!(in_readable_segment, "stackmaps range not in a readable segment");
}

#[test]
fn stackmaps_nopie_ld_fragment_links_without_rodata_and_exports_symbols() {
  let _rt = TestRuntimeGuard::new();
  let Some(lld_flag) = lld_flag() else {
    eprintln!("skipping: lld not found in PATH (need ld.lld-18 or ld.lld)");
    return;
  };
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang not found in PATH (need clang-18 or clang)");
    return;
  };

  const START_SYM: &str = "__start_llvm_stackmaps";
  const STOP_SYM: &str = "__stop_llvm_stackmaps";
  // Generic alias.
  const GENERIC_START_SYM: &str = "__stackmaps_start";
  const GENERIC_END_SYM: &str = "__stackmaps_end";
  // Legacy aliases (kept for compatibility with older tooling).
  const LEGACY_START_SYM: &str = "__llvm_stackmaps_start";
  const LEGACY_END_SYM: &str = "__llvm_stackmaps_end";
  const LEGACY_FASTR_START_SYM: &str = "__fastr_stackmaps_start";
  const LEGACY_FASTR_END_SYM: &str = "__fastr_stackmaps_end";

  let td = tempfile::tempdir().unwrap();
  let obj = compile_obj_nopie(clang, td.path());

  let exe = td.path().join("a.out");
  let script = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("link")
    .join("stackmaps_nopie.ld");

  // Link a minimal non-PIE executable (no stdlib/CRT) using the non-PIE stackmaps
  // script fragment. We set the entrypoint to our `f` symbol since we never run
  // the binary; we only inspect its sections/symbols.
  let status = Command::new(clang)
    .arg("-nostdlib")
    .arg(lld_flag)
    .arg("-no-pie")
    .arg("-Wl,-e,f")
    // Ensure `.llvm_stackmaps` is still retained under dead-section elimination.
    // The linker script fragment uses `KEEP(*(.llvm_stackmaps ...))` to prevent
    // GC from discarding the section even if it's otherwise unreferenced.
    .arg("-Wl,--gc-sections")
    .arg(format!("-Wl,-T,{}", script.display()))
    .arg("-o")
    .arg(&exe)
    .arg(&obj)
    .status()
    .unwrap();
  assert!(status.success(), "linking failed");

  let bytes = fs::read(&exe).unwrap();
  let file = object::File::parse(&*bytes).unwrap();

  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section (was stackmaps GC'd?)");

  let section_addr = section.address();
  let section_size = section.size();
  assert!(section_size > 0, "expected non-empty .llvm_stackmaps");

  let (start, start_scope) = find_symbol(&file, START_SYM).expect("missing __start_llvm_stackmaps");
  let (stop, stop_scope) = find_symbol(&file, STOP_SYM).expect("missing __stop_llvm_stackmaps");
  let (generic_start, generic_start_scope) =
    find_symbol(&file, GENERIC_START_SYM).expect("missing __stackmaps_start");
  let (generic_end, generic_end_scope) = find_symbol(&file, GENERIC_END_SYM).expect("missing __stackmaps_end");
  let (legacy_start, legacy_start_scope) =
    find_symbol(&file, LEGACY_START_SYM).expect("missing __llvm_stackmaps_start");
  let (legacy_end, legacy_end_scope) =
    find_symbol(&file, LEGACY_END_SYM).expect("missing __llvm_stackmaps_end");
  let (fastr_start, fastr_start_scope) =
    find_symbol(&file, LEGACY_FASTR_START_SYM).expect("missing __fastr_stackmaps_start");
  let (fastr_end, fastr_end_scope) =
    find_symbol(&file, LEGACY_FASTR_END_SYM).expect("missing __fastr_stackmaps_end");

  assert_ne!(
    start_scope,
    SymbolScope::Compilation,
    "{START_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    stop_scope,
    SymbolScope::Compilation,
    "{STOP_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    generic_start_scope,
    SymbolScope::Compilation,
    "{GENERIC_START_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    generic_end_scope,
    SymbolScope::Compilation,
    "{GENERIC_END_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    legacy_start_scope,
    SymbolScope::Compilation,
    "{LEGACY_START_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    legacy_end_scope,
    SymbolScope::Compilation,
    "{LEGACY_END_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    fastr_start_scope,
    SymbolScope::Compilation,
    "{LEGACY_FASTR_START_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    fastr_end_scope,
    SymbolScope::Compilation,
    "{LEGACY_FASTR_END_SYM} must be globally linkable (not a local symbol)"
  );

  assert!(
    stop > start,
    "invalid stackmaps symbol range: start=0x{start:x} stop=0x{stop:x}"
  );
  assert!(
    section_addr <= start && stop <= section_addr + section_size,
    "stackmaps symbol range must be contained in .llvm_stackmaps (section_addr=0x{section_addr:x} size=0x{section_size:x} start=0x{start:x} stop=0x{stop:x})"
  );

  assert_eq!(generic_start, start, "generic start symbol must match");
  assert_eq!(generic_end, stop, "generic end symbol must match");
  assert_eq!(legacy_start, start, "legacy start symbol must match");
  assert_eq!(legacy_end, stop, "legacy end symbol must match");
  assert_eq!(fastr_start, start, "fastr start symbol must match");
  assert_eq!(fastr_end, stop, "fastr end symbol must match");

  // Ensure the linker script actually retained our bytes (not just the symbols).
  let data = section.data().expect("read .llvm_stackmaps bytes");
  let off = usize::try_from(start - section_addr).expect("offset fits usize");
  let len = usize::try_from(stop - start).expect("len fits usize");
  assert!(off + len <= data.len(), "stackmaps range out of bounds");
  assert!(
    data[off..].starts_with(&[1, 2, 3, 4]),
    "expected .llvm_stackmaps payload to be preserved"
  );

  // For non-PIE output, stackmaps should live in read-only memory.
  let mut in_ro_segment = false;
  let section_end = stop;
  for seg in file.segments() {
    let seg_addr = seg.address();
    let seg_end = seg_addr + seg.size();
    let flags = seg.flags();
    if seg_addr <= start
      && section_end <= seg_end
      && segment_is_readable(flags)
      && !segment_is_writable(flags)
    {
      in_ro_segment = true;
      break;
    }
  }
  assert!(in_ro_segment, "stackmaps range not in a read-only segment");
}
