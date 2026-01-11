#![cfg(target_os = "linux")]

use object::{Object, ObjectSection};
use runtime_native::stackmaps::{Location, StackSize};
use runtime_native::statepoints::StatepointRecord;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn tool_available(tool: &str) -> bool {
  Command::new(tool)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok()
}

fn run_success(mut cmd: Command) -> String {
  let cmd_str = format!("{cmd:?}");
  let out = cmd.output().unwrap_or_else(|e| panic!("failed to run {cmd_str}: {e}"));
  if !out.status.success() {
    panic!(
      "command failed: {cmd_str}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
      out.status,
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr),
    );
  }
  String::from_utf8(out.stdout).expect("command output must be UTF-8")
}

fn parse_call_return_offsets_from_objdump(objdump: &str, fn_name: &str, call_prefix: &str) -> Vec<u64> {
  let mut in_fn = false;
  let mut fn_start = 0u64;
  let mut saw_call = false;
  let mut ret_offsets = Vec::new();

  for line in objdump.lines() {
    let line_trimmed = line.trim_end();

    if line_trimmed.ends_with(&format!("<{fn_name}>:")) {
      let addr_str = line_trimmed
        .split_whitespace()
        .next()
        .unwrap_or_default();
      fn_start = u64::from_str_radix(addr_str, 16)
        .unwrap_or_else(|_| panic!("failed to parse function address in line: {line_trimmed}"));
      in_fn = true;
      saw_call = false;
      continue;
    }

    if !in_fn {
      continue;
    }

    // Stop if we hit the next function header.
    if line_trimmed.contains('<') && line_trimmed.ends_with(">:") {
      break;
    }

    let inst_line = line_trimmed.trim_start();
    let Some((addr_str, rest)) = inst_line.split_once(':') else {
      continue;
    };
    let addr = u64::from_str_radix(addr_str.trim(), 16)
      .unwrap_or_else(|_| panic!("failed to parse instruction address in line: {line_trimmed}"));

    // If the previous instruction was a call, this address is the return address.
    if saw_call {
      ret_offsets.push(addr - fn_start);
      saw_call = false;
    }

    let inst = rest.trim_start();
    if inst.starts_with(call_prefix) {
      saw_call = true;
    }
  }

  ret_offsets
}

#[test]
fn llvm18_statepoint_stackmaps_codegen_aarch64() {
  for tool in ["opt-18", "llc-18", "llvm-objdump-18"] {
    if !tool_available(tool) {
      eprintln!("skipping: {tool} not available in PATH");
      return;
    }
  }

  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let ir = manifest_dir.join("tests/fixtures/ir/statepoint_gcroot2.ll");
  assert!(ir.exists(), "missing IR fixture: {ir:?}");

  let tmp = tempfile::tempdir().expect("create tempdir");
  let rewritten = tmp.path().join("statepoint_aarch64_rewritten.ll");
  let obj = tmp.path().join("statepoint_aarch64.o");

  // 1) Rewrite gcroot -> statepoint so `.llvm_stackmaps` contains statepoint records.
  let mut opt = Command::new("opt-18");
  opt
    .arg("-mtriple=aarch64-unknown-linux-gnu")
    .arg("-passes=rewrite-statepoints-for-gc")
    .arg("-S")
    .arg(&ir)
    .arg("-o")
    .arg(&rewritten);
  run_success(opt);

  // 2) Compile to an AArch64 object with frame pointers enabled (required for FP walking).
  let mut llc = Command::new("llc-18");
  llc
    .arg("-O0")
    .arg("-filetype=obj")
    // runtime-native requires statepoint roots to be spilled to stack slots.
    .arg("--fixup-allow-gcptr-in-csr=false")
    .arg("--fixup-max-csr-statepoints=0")
    .arg("-mtriple=aarch64-unknown-linux-gnu")
    .arg("-mcpu=generic")
    .arg("-frame-pointer=all")
    .arg(&rewritten)
    .arg("-o")
    .arg(&obj);
  run_success(llc);

  // 3) Extract `.llvm_stackmaps` and parse.
  let obj_bytes = std::fs::read(&obj).expect("read object");
  let obj_file = object::File::parse(&*obj_bytes).expect("parse object");
  let stackmaps_bytes = obj_file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section")
    .data()
    .expect("read .llvm_stackmaps section data");

  let stackmaps = runtime_native::stackmaps::StackMap::parse(stackmaps_bytes)
    .expect("parse .llvm_stackmaps");

  assert_eq!(stackmaps.version, 3);
  assert_eq!(stackmaps.functions.len(), 1);
  assert_eq!(
    stackmaps.functions[0].stack_size,
    StackSize::Known(48),
    "unexpected AArch64 stack_size; if LLVM output changed, update this test"
  );
  assert_eq!(stackmaps.records.len(), 2);

  // 4) Verify statepoint root locations use `Indirect [SP + off]` with DWARF reg 31 (SP).
  for rec in &stackmaps.records {
    let sp = StatepointRecord::new(rec).expect("decode statepoint record");
    assert_eq!(sp.gc_pairs().len(), 2);
    for pair in sp.gc_pairs() {
      for (kind, loc) in [("base", &pair.base), ("derived", &pair.derived)] {
        match loc {
          Location::Indirect { size, dwarf_reg, .. } => {
            assert_eq!(*size, 8);
            assert_eq!(
              *dwarf_reg, 31,
              "expected AArch64 SP dwarf_reg=31 for {kind} location"
            );
          }
          other => panic!("expected Indirect gc root {kind} location, got {other:?}"),
        }
      }
    }
  }

  // 5) Validate instruction_offset corresponds to the call return address.
  let mut objdump = Command::new("llvm-objdump-18");
  objdump.arg("-d").arg("--no-show-raw-insn").arg(&obj);
  let disasm = run_success(objdump);

  let expected_return_offsets = parse_call_return_offsets_from_objdump(&disasm, "statepoint_gcroot2", "bl")
    .into_iter()
    .take(stackmaps.records.len())
    .collect::<Vec<_>>();
  assert_eq!(
    expected_return_offsets.len(),
    stackmaps.records.len(),
    "expected at least {} call return offsets in disassembly; got {}\n{disasm}",
    stackmaps.records.len(),
    expected_return_offsets.len()
  );

  let mut stackmap_offsets: Vec<u64> = stackmaps
    .records
    .iter()
    .map(|r| r.instruction_offset as u64)
    .collect();
  stackmap_offsets.sort_unstable();

  let mut expected = expected_return_offsets;
  expected.sort_unstable();

  assert_eq!(
    stackmap_offsets, expected,
    "stackmap instruction offsets should be the call return addresses\n\
     disassembly:\n{disasm}"
  );
}
