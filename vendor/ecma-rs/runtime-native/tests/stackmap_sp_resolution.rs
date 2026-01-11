//! Integration test for StackMap SP-relative location resolution.
//!
//! LLVM StackMap `Indirect [SP + off]` locations for statepoints are based on the
//! *function's* SP at the safepoint (after prologue / local allocation). When
//! walking frames via frame pointers we typically only have FP, so we need the
//! StackMap function record's `stack_size` to reconstruct that SP.

use object::{Object, ObjectSection};
use runtime_native::stackmaps::{CallSite, StackMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Arch {
  X86_64,
  Aarch64,
}

impl Arch {
  fn triple(self) -> &'static str {
    match self {
      Arch::X86_64 => "x86_64-unknown-linux-gnu",
      Arch::Aarch64 => "aarch64-unknown-linux-gnu",
    }
  }

  fn frame_record_size(self) -> u64 {
    match self {
      Arch::X86_64 => 8,
      Arch::Aarch64 => 16,
    }
  }
}

fn run_success(cmd: &mut Command) {
  let out = cmd.output().unwrap_or_else(|e| panic!("failed to run {cmd:?}: {e}"));
  if !out.status.success() {
    panic!(
      "command failed: {cmd:?}\nstdout:\n{}\nstderr:\n{}",
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr)
    );
  }
}

fn capture_stdout(cmd: &mut Command) -> String {
  let out = cmd.output().unwrap_or_else(|e| panic!("failed to run {cmd:?}: {e}"));
  if !out.status.success() {
    panic!(
      "command failed: {cmd:?}\nstdout:\n{}\nstderr:\n{}",
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr)
    );
  }
  String::from_utf8(out.stdout).expect("stdout was not valid UTF-8")
}

fn write_minimal_statepoint_input_ir(triple: &str) -> String {
  // Keep this IR tiny and stable:
  // - `rewrite-statepoints-for-gc` will turn the call into a gc.statepoint and emit stackmaps.
  // - `ptr addrspace(1)` marks GC pointers for statepoint-based strategies (e.g. coreclr).
  format!(
    r#"target triple = "{triple}"

declare void @callee(ptr addrspace(1))

define ptr addrspace(1) @foo(ptr addrspace(1) %p) gc "coreclr" {{
entry:
  call void @callee(ptr addrspace(1) %p)
  ret ptr addrspace(1) %p
}}
"#
  )
}

fn build_obj(tmp: &Path, arch: Arch) -> PathBuf {
  let input_ll = tmp.join("input.ll");
  let rewritten_ll = tmp.join("rewritten.ll");
  let obj = tmp.join(format!("statepoint_{}.o", arch.triple()));

  fs::write(&input_ll, write_minimal_statepoint_input_ir(arch.triple())).unwrap();

  run_success(
    Command::new("opt-18")
      .arg("-S")
      .arg(format!("-mtriple={}", arch.triple()))
      .arg("-passes=rewrite-statepoints-for-gc")
      .arg(&input_ll)
      .arg("-o")
      .arg(&rewritten_ll),
  );

  run_success(
    Command::new("llc-18")
      .arg("-O0")
      .arg("-filetype=obj")
      .arg("-frame-pointer=all")
      // runtime-native requires statepoint roots to be spilled to stack slots.
      .arg("--fixup-allow-gcptr-in-csr=false")
      .arg("--fixup-max-csr-statepoints=0")
      .arg(format!("-mtriple={}", arch.triple()))
      .arg(&rewritten_ll)
      .arg("-o")
      .arg(&obj),
  );

  obj
}

fn stackmap_section_bytes(obj_bytes: &[u8]) -> &[u8] {
  let obj = object::File::parse(obj_bytes).expect("parse object");
  let section = obj
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  section.data().expect("read .llvm_stackmaps bytes")
}

fn parse_hex_u64(mut s: &str) -> Option<u64> {
  s = s.trim();
  s = s.trim_end_matches(|c: char| c == ',' || c == ']');
  s = s.strip_prefix("0x").unwrap_or(s);
  u64::from_str_radix(s, 16).ok()
}

fn parse_imm_i64(s: &str) -> Option<i64> {
  let s = s.trim();
  let s = s.trim_end_matches(|c: char| c == ',' || c == ']');
  let (neg, s) = s.strip_prefix('-').map(|s| (true, s)).unwrap_or((false, s));
  let val = if let Some(hex) = s.strip_prefix("0x") {
    u64::from_str_radix(hex, 16).ok()? as i64
  } else {
    s.parse::<i64>().ok()?
  };
  Some(if neg { -val } else { val })
}

fn disasm_fp_relative_slot_offset(arch: Arch, disasm: &str) -> i32 {
  match arch {
    Arch::X86_64 => {
      // Example (intel syntax):
      //   mov qword ptr [rbp - 0x8], rdi
      for line in disasm.lines() {
        if let Some(i) = line.find("[rbp - 0x") {
          let rest = &line[i + "[rbp - 0x".len()..];
          let hex = rest.split(']').next().unwrap();
          let off = parse_hex_u64(hex).unwrap() as i64;
          return i32::try_from(-off).unwrap();
        }
      }
      panic!("failed to find an [rbp - off] spill slot in x86_64 disassembly:\n{disasm}");
    }

    Arch::Aarch64 => {
      // Typical patterns (LLVM 18, -O0):
      //   add x29, sp, #0x10
      //   str x0, [sp, #0x8]
      //
      // Or sometimes fp-relative addressing:
      //   str x0, [x29, #-0x8]
      let mut fp_from_sp: Option<i64> = None;

      // First pass: find the FP establishment.
      for line in disasm.lines() {
        let Some((_addr, insn)) = line.split_once(':') else {
          continue;
        };
        let insn = insn.trim();
        if insn.starts_with("add") && insn.contains("x29, sp, #") {
          let imm = insn
            .split("x29, sp, #")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
          fp_from_sp = Some(parse_imm_i64(imm).unwrap());
          break;
        }
        if insn.starts_with("mov") && insn.contains("x29, sp") {
          fp_from_sp = Some(0);
          break;
        }
      }

      let fp_from_sp = fp_from_sp.expect("failed to find FP establishment in aarch64 prologue");

      // Second pass: find the spill slot for x0.
      for line in disasm.lines() {
        let Some((_addr, insn)) = line.split_once(':') else {
          continue;
        };
        let insn = insn.trim();

        // SP-relative spill.
        if (insn.starts_with("str") || insn.starts_with("ldr")) && insn.contains("x0, [sp, #") {
          let imm = insn
            .split("x0, [sp, #")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
          let sp_off = parse_imm_i64(imm).unwrap();
          let fp_rel = sp_off - fp_from_sp;
          return i32::try_from(fp_rel).unwrap();
        }

        // FP-relative spill (usually negative).
        if (insn.starts_with("str") || insn.starts_with("ldr")) && insn.contains("x0, [x29, #") {
          let imm = insn
            .split("x0, [x29, #")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
          let fp_rel = parse_imm_i64(imm).unwrap();
          return i32::try_from(fp_rel).unwrap();
        }
      }

      panic!("failed to find an x0 spill slot in aarch64 disassembly:\n{disasm}");
    }
  }
}

fn assert_stackmap_fp_offsets_match_disasm(arch: Arch) {
  let tmp = tempfile::tempdir().unwrap();
  let obj_path = build_obj(tmp.path(), arch);
  let obj_bytes = fs::read(&obj_path).unwrap();

  let sm_bytes = stackmap_section_bytes(&obj_bytes);
  let stackmap = StackMap::parse(sm_bytes).expect("parse stackmap blob");
  assert!(
    !stackmap.functions.is_empty(),
    "expected at least one stackmap function record"
  );
  assert!(
    !stackmap.records.is_empty(),
    "expected at least one stackmap callsite record"
  );

  // Find the callsite record whose decoded roots have exactly one spilled slot.
  let mut found: Option<(i32, u64)> = None;
  let mut record_index: usize = 0;
  for func in &stackmap.functions {
    let count = usize::try_from(func.record_count).expect("record_count overflowed usize");
    for _ in 0..count {
      let record = stackmap
        .records
        .get(record_index)
        .expect("function record_count exceeds records length");
      record_index += 1;

      let callsite = CallSite {
        stack_size: func.stack_size,
        record,
      };
      let fp_offsets = callsite
        .gc_root_rbp_offsets_strict()
        .expect("decode + normalize GC roots");

      if fp_offsets.len() == 1 {
        if found.is_some() {
          panic!("found multiple candidate callsites with exactly one GC root slot");
        }
        found = Some((fp_offsets[0], callsite.stack_size));
      }
    }
  }
  assert_eq!(
    record_index,
    stackmap.records.len(),
    "function record_count sum did not match records.len()"
  );

  let (fp_off_from_stackmap, stack_size) = found.expect("failed to find a callsite with 1 root slot");

  let mut objdump = Command::new("llvm-objdump-18");
  objdump.arg("-d").arg("--no-show-raw-insn");
  if arch == Arch::X86_64 {
    objdump.arg("-M").arg("intel");
  }
  let disasm = capture_stdout(objdump.arg(&obj_path));

  let fp_off_from_disasm = disasm_fp_relative_slot_offset(arch, &disasm);
  assert_eq!(
    fp_off_from_stackmap, fp_off_from_disasm,
    "stackmap FP-relative offset does not match disassembly\narch={arch:?}\n\
     stack_size={}\n\
     frame_record_size={}\n\
     fp_off(stackmap)={fp_off_from_stackmap}\n\
     fp_off(disasm)={fp_off_from_disasm}\n\
     disasm:\n{disasm}",
    stack_size,
    arch.frame_record_size()
  );
}

#[test]
fn statepoint_stackmap_sp_locations_resolve_via_stack_size_x86_64() {
  assert_stackmap_fp_offsets_match_disasm(Arch::X86_64);
}

#[test]
fn statepoint_stackmap_sp_locations_resolve_via_stack_size_aarch64() {
  assert_stackmap_fp_offsets_match_disasm(Arch::Aarch64);
}
