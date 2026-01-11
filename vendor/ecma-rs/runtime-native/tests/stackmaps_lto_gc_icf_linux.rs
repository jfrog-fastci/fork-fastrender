#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol};
use runtime_native::stackmaps::{parse_all_stackmaps, StackMap, StackMaps};
use runtime_native::statepoint_verify::LLVM_STATEPOINT_PATCHPOINT_ID;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LtoMode {
  None,
  Thin,
  Full,
}

impl LtoMode {
  fn suffix(self) -> &'static str {
    match self {
      Self::None => "",
      Self::Thin => ".thinlto",
      Self::Full => ".lto",
    }
  }

  fn clang_flag(self) -> Option<&'static str> {
    match self {
      Self::None => None,
      Self::Thin => Some("-flto=thin"),
      Self::Full => Some("-flto=full"),
    }
  }
}

fn command_works(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok()
}

fn find_clang() -> Option<&'static str> {
  for cand in ["clang-18", "clang"] {
    if command_works(cand) {
      return Some(cand);
    }
  }
  None
}

fn lld_flag() -> Option<&'static str> {
  // Prefer version-suffixed lld if installed.
  if command_works("ld.lld-18") {
    Some("-fuse-ld=lld-18")
  } else if command_works("ld.lld") {
    Some("-fuse-ld=lld")
  } else {
    None
  }
}

fn run_ok(mut cmd: Command, what: &str) {
  let output = cmd.output().unwrap_or_else(|err| panic!("failed to spawn {what}: {err}"));
  assert!(
    output.status.success(),
    "{what} failed.\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
    output.status,
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );
}

fn write_file(path: &Path, contents: &str) {
  fs::write(path, contents).unwrap();
}

fn compile_ir(clang: &str, out_dir: &Path, name: &str, ir: &str, lto: LtoMode) -> PathBuf {
  let ll_path = out_dir.join(format!("{name}.ll"));
  write_file(&ll_path, ir);

  let obj_path = out_dir.join(format!("{name}{}.o", lto.suffix()));
  let mut cmd = Command::new(clang);
  cmd.arg("-c")
    .arg("-O2")
    .args(["-ffunction-sections", "-fdata-sections"])
    .arg(&ll_path)
    .arg("-o")
    .arg(&obj_path);
  if let Some(flag) = lto.clang_flag() {
    cmd.arg(flag);
  }
  run_ok(cmd, &format!("compile {name}.ll"));
  assert!(obj_path.exists(), "missing output object {}", obj_path.display());
  obj_path
}

fn compile_c(clang: &str, out_dir: &Path, name: &str, c: &str, lto: LtoMode) -> PathBuf {
  let c_path = out_dir.join(format!("{name}.c"));
  write_file(&c_path, c);

  let obj_path = out_dir.join(format!("{name}{}.o", lto.suffix()));
  let mut cmd = Command::new(clang);
  cmd.arg("-c")
    .arg("-O2")
    .args(["-ffunction-sections", "-fdata-sections"])
    .arg(&c_path)
    .arg("-o")
    .arg(&obj_path);
  if let Some(flag) = lto.clang_flag() {
    cmd.arg(flag);
  }
  run_ok(cmd, &format!("compile {name}.c"));
  assert!(obj_path.exists(), "missing output object {}", obj_path.display());
  obj_path
}

fn find_symbol<'data>(file: &object::File<'data>, name: &str) -> Option<u64> {
  for sym in file.symbols() {
    if sym.name().ok() == Some(name) {
      return Some(sym.address());
    }
  }
  for sym in file.dynamic_symbols() {
    if sym.name().ok() == Some(name) {
      return Some(sym.address());
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

fn llvm_stackmaps_section(exe: &Path) -> Vec<u8> {
  let bytes = fs::read(exe).expect("read linked executable");
  let file = object::File::parse(&*bytes).expect("parse linked executable");
  let section = file
    .section_by_name(".data.rel.ro.llvm_stackmaps")
    .or_else(|| file.section_by_name(".llvm_stackmaps"))
    .expect("missing stackmaps section (linker GC?)");
  section
    .data()
    .expect("read stackmaps section bytes")
    .to_vec()
}

fn callsites_for_stackmap(sm: &StackMap) -> Vec<(u64, u64)> {
  let mut out = Vec::new();
  let mut record_index: usize = 0;
  for f in &sm.functions {
    let rc = usize::try_from(f.record_count).expect("record_count fits usize");
    for _ in 0..rc {
      let rec = &sm.records[record_index];
      let pc = f
        .address
        .checked_add(rec.instruction_offset as u64)
        .expect("pc overflow");
      out.push((pc, rec.patchpoint_id));
      record_index += 1;
    }
  }
  out
}

fn validate_exe(exe: &Path, expect_linker_symbols: bool) {
  // Section present + readable.
  let bytes = fs::read(exe).expect("read linked executable");
  let file = object::File::parse(&*bytes).expect("parse linked executable");
  let section = file
    .section_by_name(".data.rel.ro.llvm_stackmaps")
    .or_else(|| file.section_by_name(".llvm_stackmaps"))
    .expect("missing stackmaps section (linker GC?)");
  let section_addr = section.address();
  let section_size = section.size();
  assert!(section_size > 0, "expected non-empty stackmaps section");

  if expect_linker_symbols {
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

    let start = find_symbol(&file, START_SYM).expect("missing __start_llvm_stackmaps");
    let stop = find_symbol(&file, STOP_SYM).expect("missing __stop_llvm_stackmaps");
    let generic_start = find_symbol(&file, GENERIC_START_SYM).expect("missing __stackmaps_start");
    let generic_end = find_symbol(&file, GENERIC_END_SYM).expect("missing __stackmaps_end");
    let legacy_start = find_symbol(&file, LEGACY_START_SYM).expect("missing __llvm_stackmaps_start");
    let legacy_end = find_symbol(&file, LEGACY_END_SYM).expect("missing __llvm_stackmaps_end");
    let fastr_start =
      find_symbol(&file, LEGACY_FASTR_START_SYM).expect("missing __fastr_stackmaps_start");
    let fastr_end =
      find_symbol(&file, LEGACY_FASTR_END_SYM).expect("missing __fastr_stackmaps_end");

    assert_eq!(
      start, section_addr,
      "start symbol must equal the stackmaps section virtual address"
    );
    assert_eq!(
      stop.checked_sub(start).unwrap(),
      section_size,
      "stop-start must equal the stackmaps section size"
    );

    assert_eq!(generic_start, start, "generic start symbol must match");
    assert_eq!(generic_end, stop, "generic end symbol must match");
    assert_eq!(legacy_start, start, "legacy start symbol must match");
    assert_eq!(legacy_end, stop, "legacy end symbol must match");
    assert_eq!(fastr_start, start, "fastr start symbol must match");
    assert_eq!(fastr_end, stop, "fastr end symbol must match");

    // Ensure the section is backed by a readable load segment so the runtime can
    // read the bytes directly from memory.
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

  // Parse + validate (this runs statepoint verification in debug builds).
  let stackmaps_bytes = section
    .data()
    .expect("read stackmaps section bytes")
    .to_vec();
  let blobs = parse_all_stackmaps(&stackmaps_bytes).expect("parse concatenated stackmap blobs");
  assert!(!blobs.is_empty(), "expected at least one stackmap blob");

  // Ensure we produced statepoint records (not just patchpoints) so the runtime
  // verifier is exercised.
  let statepoint_records: usize = blobs
    .iter()
    .map(|sm| sm.records.iter().filter(|r| r.patchpoint_id == LLVM_STATEPOINT_PATCHPOINT_ID).count())
    .sum();
  assert!(statepoint_records > 0, "expected at least one gc.statepoint record");

  let mut expected_callsites: Vec<(u64, u64)> = Vec::new();
  for sm in &blobs {
    expected_callsites.extend(callsites_for_stackmap(sm));
  }
  assert!(
    !expected_callsites.is_empty(),
    "expected at least one callsite in stackmaps section"
  );

  let index = StackMaps::parse(&stackmaps_bytes).expect("parse + index stackmaps");
  for (pc, patchpoint_id) in expected_callsites {
    let callsite = index
      .lookup(pc)
      .unwrap_or_else(|| panic!("missing indexed callsite for pc=0x{pc:x}"));
    assert_eq!(
      callsite.record.patchpoint_id, patchpoint_id,
      "wrong record for pc=0x{pc:x}"
    );
  }

  // Also ensure we can independently load the bytes via the helper.
  let bytes2 = llvm_stackmaps_section(exe);
  assert_eq!(bytes2, stackmaps_bytes);
}

const MOD_A: &str = r#"
; ModuleID = 'lto_gc_icf_a'
target triple = "x86_64-pc-linux-gnu"

declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32, i32)

define void @callee() {
entry:
  ret void
}

define i64 @sp_a(ptr addrspace(1) %p1, ptr addrspace(1) %p2) #0 gc "coreclr" {
entry:
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
      i64 2882400000, i32 0,
      ptr elementtype(void ()) @callee,
      i32 0, i32 0,
      i32 0, i32 0
    ) [ "gc-live"(ptr addrspace(1) %p1, ptr addrspace(1) %p2) ]
  %p1r = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 0)
  %p2r = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 1, i32 1)

  %i1 = ptrtoint ptr addrspace(1) %p1r to i64
  %i2 = ptrtoint ptr addrspace(1) %p2r to i64
  %sum = add i64 %i1, %i2
  ret i64 %sum
}

attributes #0 = { noinline }
"#;

const MOD_B: &str = r#"
; ModuleID = 'lto_gc_icf_b'
target triple = "x86_64-pc-linux-gnu"

declare void @callee()
declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32, i32)

define i64 @sp_b(ptr addrspace(1) %p1, ptr addrspace(1) %p2) #0 gc "coreclr" {
entry:
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
      i64 2882400000, i32 0,
      ptr elementtype(void ()) @callee,
      i32 0, i32 0,
      i32 0, i32 0
    ) [ "gc-live"(ptr addrspace(1) %p1, ptr addrspace(1) %p2) ]
  %p1r = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 0)
  %p2r = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 1, i32 1)

  %i1 = ptrtoint ptr addrspace(1) %p1r to i64
  %i2 = ptrtoint ptr addrspace(1) %p2r to i64
  %sum = add i64 %i1, %i2
  ret i64 %sum
}

attributes #0 = { noinline }
"#;

const MAIN_C: &str = r#"
#include <stdint.h>

extern int64_t sp_a(void* p1, void* p2);
extern int64_t sp_b(void* p1, void* p2);

// Ensure the linked executable has a `.data` output section even under `--gc-sections`:
// `link/stackmaps.ld` uses `INSERT BEFORE .data` as its anchor.
volatile int64_t g_data = 1;

int main(void) {
  volatile int64_t a = sp_a((void*)0x1000, (void*)0x2000);
  volatile int64_t b = sp_b((void*)0x3000, (void*)0x4000);
  return (int)(a + b + g_data);
}
"#;

#[derive(Debug, Clone, Copy)]
struct LinkConfig {
  name: &'static str,
  lto: LtoMode,
  gc_sections: bool,
  keep_stackmaps: bool,
  icf_all: bool,
}

#[test]
fn stackmaps_survive_lto_gc_sections_and_icf() {
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang-18 not found in PATH");
    return;
  };

  let lld_flag = lld_flag();
  let lld = lld_flag.is_some();

  let cfgs: &[LinkConfig] = &[
    LinkConfig {
      name: "baseline",
      lto: LtoMode::None,
      gc_sections: false,
      keep_stackmaps: false,
      icf_all: false,
    },
    LinkConfig {
      name: "gc_keep",
      lto: LtoMode::None,
      gc_sections: true,
      keep_stackmaps: true,
      icf_all: false,
    },
    LinkConfig {
      name: "thinlto_keep",
      lto: LtoMode::Thin,
      gc_sections: false,
      keep_stackmaps: true,
      icf_all: false,
    },
    LinkConfig {
      name: "thinlto_gc_keep",
      lto: LtoMode::Thin,
      gc_sections: true,
      keep_stackmaps: true,
      icf_all: false,
    },
    LinkConfig {
      name: "thinlto_gc_keep_icf",
      lto: LtoMode::Thin,
      gc_sections: true,
      keep_stackmaps: true,
      icf_all: true,
    },
    LinkConfig {
      name: "fulllto_keep",
      lto: LtoMode::Full,
      gc_sections: false,
      keep_stackmaps: true,
      icf_all: false,
    },
    // Full LTO + ICF can fold functions and produce duplicate callsite PCs in
    // `.llvm_stackmaps`. `StackMaps::parse` is expected to deduplicate identical
    // records so lookups remain unambiguous.
    LinkConfig {
      name: "fulllto_keep_icf",
      lto: LtoMode::Full,
      gc_sections: false,
      keep_stackmaps: true,
      icf_all: true,
    },
    LinkConfig {
      name: "fulllto_gc_keep",
      lto: LtoMode::Full,
      gc_sections: true,
      keep_stackmaps: true,
      icf_all: false,
    },
    LinkConfig {
      name: "fulllto_gc_keep_icf",
      lto: LtoMode::Full,
      gc_sections: true,
      keep_stackmaps: true,
      icf_all: true,
    },
  ];

  let td = tempfile::tempdir().expect("create tempdir");
  let out_dir = td.path();

  let a_o = compile_ir(clang, out_dir, "a", MOD_A, LtoMode::None);
  let b_o = compile_ir(clang, out_dir, "b", MOD_B, LtoMode::None);
  let main_o = compile_c(clang, out_dir, "main", MAIN_C, LtoMode::None);

  let a_thin_o = compile_ir(clang, out_dir, "a", MOD_A, LtoMode::Thin);
  let b_thin_o = compile_ir(clang, out_dir, "b", MOD_B, LtoMode::Thin);
  let main_thin_o = compile_c(clang, out_dir, "main", MAIN_C, LtoMode::Thin);

  let a_full_o = compile_ir(clang, out_dir, "a", MOD_A, LtoMode::Full);
  let b_full_o = compile_ir(clang, out_dir, "b", MOD_B, LtoMode::Full);
  let main_full_o = compile_c(clang, out_dir, "main", MAIN_C, LtoMode::Full);

  let script = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("link")
    .join("stackmaps.ld");

  for cfg in cfgs {
    if (cfg.lto != LtoMode::None || cfg.icf_all) && !lld {
      eprintln!("skipping {}: lld not found in PATH", cfg.name);
      continue;
    }

    let exe = out_dir.join(format!("{}.out", cfg.name));
    let mut cmd = Command::new(clang);
    cmd.arg("-no-pie");

    // Match production (clang + lld) when available. LTO/ICF require lld.
    if let Some(flag) = lld_flag {
      if cfg.lto != LtoMode::None || cfg.keep_stackmaps || cfg.gc_sections || cfg.icf_all {
        cmd.arg(flag);
      }
    }

    if let Some(flag) = cfg.lto.clang_flag() {
      cmd.arg(flag);
    }
    if cfg.gc_sections {
      cmd.arg("-Wl,--gc-sections");
    }
    if cfg.keep_stackmaps {
      cmd.arg(format!("-Wl,-T,{}", script.display()));
    }
    if cfg.icf_all {
      cmd.arg("-Wl,--icf=all");
    }

    cmd.arg("-o").arg(&exe);

    match cfg.lto {
      LtoMode::None => {
        cmd.arg(&main_o).arg(&a_o).arg(&b_o);
      }
      LtoMode::Thin => {
        cmd.arg(&main_thin_o).arg(&a_thin_o).arg(&b_thin_o);
      }
      LtoMode::Full => {
        cmd.arg(&main_full_o).arg(&a_full_o).arg(&b_full_o);
      }
    };

    run_ok(cmd, &format!("link {}", cfg.name));
    assert!(exe.exists(), "missing output executable {}", exe.display());

    validate_exe(&exe, cfg.keep_stackmaps);
  }
}
