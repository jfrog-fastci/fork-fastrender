#![cfg(target_os = "linux")]

use native_js::link::{LLVM_STACKMAPS_START_SYM, LLVM_STACKMAPS_STOP_SYM};
use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol, SymbolScope};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn write_file(path: &Path, contents: &str) {
  fs::write(path, contents).unwrap();
}

fn clang() -> &'static str {
  for cand in ["clang-18", "clang"] {
    if Command::new(cand)
      .arg("--version")
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
      .status()
      .is_ok()
    {
      return cand;
    }
  }
  panic!("unable to locate clang (expected `clang-18` or `clang`)");
}

fn compile_obj(out_dir: &Path) -> PathBuf {
  let asm = r#"
.text
.globl main
main:
  xorl %eax, %eax
  ret

.section .llvm_stackmaps,"a",@progbits
__LLVM_StackMaps:
  .byte 1,2,3,4,5,6,7,8
"#;

  let asm_path = out_dir.join("stackmaps.S");
  write_file(&asm_path, asm);

  let obj_path = out_dir.join("stackmaps.o");
  let status = Command::new(clang())
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

#[test]
fn exported_stackmap_symbols_match_section_bounds() {
  let td = tempfile::tempdir().unwrap();
  let obj = compile_obj(td.path());

  let elf = td.path().join("a.out");
  native_js::link::link_elf_executable(&elf, &[obj.clone()]).unwrap();
  let data = fs::read(&elf).unwrap();
  let file = object::File::parse(&*data).unwrap();

  let section = file
    .section_by_name(".data.rel.ro.llvm_stackmaps")
    .or_else(|| file.section_by_name(".llvm_stackmaps"))
    .expect("missing stackmaps section (was it GC'd?)");

  let section_addr = section.address();
  let section_size = section.size();
  assert!(section_size > 0, "expected non-empty stackmaps section");

  let (start, start_scope) =
    find_symbol(&file, LLVM_STACKMAPS_START_SYM).expect("missing __start_llvm_stackmaps symbol");
  let (end, end_scope) =
    find_symbol(&file, LLVM_STACKMAPS_STOP_SYM).expect("missing __stop_llvm_stackmaps symbol");

  // Project / legacy symbol aliases.
  let (fastr_start, fastr_start_scope) = find_symbol(&file, "__fastr_stackmaps_start")
    .expect("missing __fastr_stackmaps_start symbol");
  let (fastr_end, fastr_end_scope) =
    find_symbol(&file, "__fastr_stackmaps_end").expect("missing __fastr_stackmaps_end symbol");
  let (llvm_start, llvm_start_scope) = find_symbol(&file, "__llvm_stackmaps_start")
    .expect("missing __llvm_stackmaps_start symbol");
  let (llvm_end, llvm_end_scope) =
    find_symbol(&file, "__llvm_stackmaps_end").expect("missing __llvm_stackmaps_end symbol");

  // Generic aliases used by tooling that doesn't want project-specific symbol names.
  let (alias_start, alias_start_scope) =
    find_symbol(&file, "__stackmaps_start").expect("missing __stackmaps_start symbol");
  let (alias_end, alias_end_scope) =
    find_symbol(&file, "__stackmaps_end").expect("missing __stackmaps_end symbol");

  assert_ne!(
    start_scope,
    SymbolScope::Compilation,
    "{LLVM_STACKMAPS_START_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    end_scope,
    SymbolScope::Compilation,
    "{LLVM_STACKMAPS_STOP_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    alias_start_scope,
    SymbolScope::Compilation,
    "__stackmaps_start must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    alias_end_scope,
    SymbolScope::Compilation,
    "__stackmaps_end must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    fastr_start_scope,
    SymbolScope::Compilation,
    "__fastr_stackmaps_start must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    fastr_end_scope,
    SymbolScope::Compilation,
    "__fastr_stackmaps_end must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    llvm_start_scope,
    SymbolScope::Compilation,
    "__llvm_stackmaps_start must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    llvm_end_scope,
    SymbolScope::Compilation,
    "__llvm_stackmaps_end must be globally linkable (not a local symbol)"
  );

  assert_eq!(
    start, section_addr,
    "start symbol must equal the stackmaps section virtual address"
  );
  assert_eq!(
    end.checked_sub(start).unwrap(),
    section_size,
    "end-start must equal the stackmaps section size"
  );
  assert_eq!(fastr_start, start, "__fastr_stackmaps_start must match {LLVM_STACKMAPS_START_SYM}");
  assert_eq!(fastr_end, end, "__fastr_stackmaps_end must match {LLVM_STACKMAPS_STOP_SYM}");
  assert_eq!(llvm_start, start, "__llvm_stackmaps_start must match {LLVM_STACKMAPS_START_SYM}");
  assert_eq!(llvm_end, end, "__llvm_stackmaps_end must match {LLVM_STACKMAPS_STOP_SYM}");
  assert_eq!(
    alias_start, start,
    "__stackmaps_start must match {LLVM_STACKMAPS_START_SYM}"
  );
  assert_eq!(
    alias_end, end,
    "__stackmaps_end must match {LLVM_STACKMAPS_STOP_SYM}"
  );

  // Optional: ensure the section is backed by a readable load segment so the runtime can read the
  // bytes directly from memory (via the start/end symbol pointers).
  let mut in_readable_segment = false;
  let section_end = section_addr + section_size;
  for seg in file.segments() {
    let seg_addr = seg.address();
    let seg_end = seg_addr + seg.size();
    let flags = seg.flags();
    if seg_addr <= section_addr && section_end <= seg_end && segment_is_readable(flags) {
      in_readable_segment = true;
      break;
    }
  }
  assert!(
    in_readable_segment,
    "stackmaps section not in a readable segment"
  );
}
