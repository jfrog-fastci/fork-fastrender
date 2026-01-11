#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol, SymbolScope};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn write_file(path: &Path, contents: &str) {
  fs::write(path, contents).unwrap();
}

fn find_clang() -> &'static str {
  for cand in ["clang-18", "clang"] {
    if Command::new(cand)
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok()
    {
      return cand;
    }
  }
  panic!("unable to locate clang (expected `clang-18` or `clang`)");
}

fn compile_obj(out_dir: &Path) -> PathBuf {
  // Intentionally avoid emitting any `.rodata` so the linker script fragment can't
  // rely on it existing.
  let asm = r#"
.text
.globl _start
_start:
  mov $60, %rax
  xor %rdi, %rdi
  syscall

.section .llvm_stackmaps,"a",@progbits
  .byte 1,2,3,4
"#;

  let asm_path = out_dir.join("stackmaps.S");
  write_file(&asm_path, asm);

  let obj_path = out_dir.join("stackmaps.o");
  let status = Command::new(find_clang())
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

fn segment_is_read_only(flags: object::SegmentFlags) -> bool {
  // PF_W/PF_R on ELF are bits 1/2 (values 2/4).
  match flags {
    object::SegmentFlags::Elf { p_flags } => (p_flags & 4) != 0 && (p_flags & 2) == 0,
    _ => true,
  }
}

#[test]
fn stackmaps_ld_fragment_links_without_rodata_and_exports_symbols() {
  const START_SYM: &str = "__fastr_stackmaps_start";
  const END_SYM: &str = "__fastr_stackmaps_end";
  // Legacy aliases that some tooling used before we standardized on the
  // `__fastr_stackmaps_*` naming.
  const LEGACY_START_SYM: &str = "__llvm_stackmaps_start";
  const LEGACY_END_SYM: &str = "__llvm_stackmaps_end";

  let td = tempfile::tempdir().unwrap();
  let obj = compile_obj(td.path());

  let exe = td.path().join("a.out");
  let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("stackmaps.ld");

  // Link a minimal binary (no stdlib/CRT) using our linker script fragment.
  let status = Command::new(find_clang())
    .arg("-nostdlib")
    .arg("-fuse-ld=lld")
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
    .expect("missing .llvm_stackmaps section (was it GC'd?)");

  let section_addr = section.address();
  let section_size = section.size();
  assert!(section_size > 0, "expected non-empty .llvm_stackmaps");

  let (start, start_scope) = find_symbol(&file, START_SYM).expect("missing __fastr_stackmaps_start");
  let (end, end_scope) = find_symbol(&file, END_SYM).expect("missing __fastr_stackmaps_end");

  let (legacy_start, legacy_start_scope) =
    find_symbol(&file, LEGACY_START_SYM).expect("missing __llvm_stackmaps_start");
  let (legacy_end, legacy_end_scope) =
    find_symbol(&file, LEGACY_END_SYM).expect("missing __llvm_stackmaps_end");

  assert_ne!(
    start_scope,
    SymbolScope::Compilation,
    "{START_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    end_scope,
    SymbolScope::Compilation,
    "{END_SYM} must be globally linkable (not a local symbol)"
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

  assert_eq!(
    start, section_addr,
    "start symbol must equal the .llvm_stackmaps section virtual address"
  );
  assert_eq!(
    end.checked_sub(start).unwrap(),
    section_size,
    "end-start must equal the .llvm_stackmaps section size"
  );

  assert_eq!(legacy_start, start, "legacy start symbol must match");
  assert_eq!(legacy_end, end, "legacy end symbol must match");

  // Ensure the section is backed by a readable load segment so the runtime can
  // read the bytes directly from memory.
  let mut in_readable_segment = false;
  let section_end = section_addr + section_size;
  for seg in file.segments() {
    let seg_addr = seg.address();
    let seg_end = seg_addr + seg.size();
    let flags = seg.flags();
    if seg_addr <= section_addr && section_end <= seg_end && segment_is_read_only(flags) {
      in_readable_segment = true;
      break;
    }
  }
  assert!(in_readable_segment, ".llvm_stackmaps not in a readable segment");
}
