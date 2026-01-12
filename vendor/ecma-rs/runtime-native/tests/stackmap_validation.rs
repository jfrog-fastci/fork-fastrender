use std::fs;
use std::process::Command;

use runtime_native::stackmaps::{Location, StackMaps};
use runtime_native::statepoints::StatepointRecord;
use runtime_native::validate_stackmaps;
use tempfile::tempdir;

fn has_cmd(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

fn find_cmd<'a>(candidates: &'a [&'a str]) -> Option<&'a str> {
  candidates.iter().copied().find(|c| has_cmd(c))
}

fn run(cmd: &mut Command) {
  let out = cmd.output().expect("failed to spawn command");
  if !out.status.success() {
    panic!(
      "command failed: {cmd:?}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
      out.status,
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr),
    );
  }
}

fn host_target_triple() -> &'static str {
  #[cfg(target_arch = "x86_64")]
  {
    "x86_64-pc-linux-gnu"
  }
  #[cfg(target_arch = "aarch64")]
  {
    "aarch64-unknown-linux-gnu"
  }
  #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
  {
    "unknown"
  }
}

fn write_statepoint_module(ll_path: &std::path::Path, src: &str) {
  fs::write(ll_path, src).expect("write .ll");
}

fn compile_statepoint_module(
  ll_path: &std::path::Path,
  bc_path: &std::path::Path,
  opt_bc_path: &std::path::Path,
) {
  run(Command::new("llvm-as-18").arg(ll_path).arg("-o").arg(bc_path));

  run(
    Command::new("opt-18")
      .arg("-passes=rewrite-statepoints-for-gc")
      .arg(bc_path)
      .arg("-o")
      .arg(opt_bc_path),
  );
}

fn llc_to_obj(opt_bc_path: &std::path::Path, obj_path: &std::path::Path, opt: &str) {
  run(
    Command::new("llc-18")
      .arg("-filetype=obj")
      .arg(opt)
      // runtime-native requires statepoint roots to be spilled to stack slots.
      .arg("--fixup-allow-gcptr-in-csr=false")
      .arg("--fixup-max-csr-statepoints=0")
      .arg("-frame-pointer=all")
      .arg(opt_bc_path)
      .arg("-o")
      .arg(obj_path),
  );
}

fn dump_stackmaps(objcopy: &str, obj_path: &std::path::Path, out_path: &std::path::Path) -> Vec<u8> {
  run(
    Command::new(objcopy)
      .arg("--dump-section")
      .arg(format!(".llvm_stackmaps={}", out_path.display()))
      .arg(obj_path),
  );
  fs::read(out_path).expect("read dumped .llvm_stackmaps")
}

fn statepoint_ir_simple(func: &str) -> String {
  let triple = host_target_triple();
  format!(
    r#"
source_filename = "stackmap_validate_simple"
target triple = "{triple}"

declare void @callee() "gc-leaf-function"
declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32, i32)

define void @{func}(ptr addrspace(1) %base) gc "coreclr" {{
entry:
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 0,
    i32 0,
    ptr elementtype(void ()) @callee,
    i32 0,
    i32 0,
    i32 0,
    i32 0) [ "gc-live"(ptr addrspace(1) %base) ]
  %_rel = call ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 0)
  ret void
}}
"#
  )
}

fn statepoint_ir_derived(func: &str, callee: &str) -> String {
  let triple = host_target_triple();
  format!(
    r#"
source_filename = "stackmap_validate_derived"
target triple = "{triple}"

declare void @{callee}() "gc-leaf-function"
declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32, i32)

define void @{func}(ptr addrspace(1) %base) gc "coreclr" {{
entry:
  %derived = getelementptr i8, ptr addrspace(1) %base, i64 16
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 0,
    i32 0,
    ptr elementtype(void ()) @{callee},
    i32 0,
    i32 0,
    i32 0,
    i32 0) [ "gc-live"(ptr addrspace(1) %base, ptr addrspace(1) %derived) ]
  %_rel = call ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 1)
  ret void
}}
"#
  )
}

fn assert_has_distinct_base_derived_pair(index: &StackMaps) {
  let pc = index
    .callsites()
    .first()
    .expect("expected at least one callsite")
    .pc;
  let callsite = index.lookup(pc).expect("lookup callsite");
  let sp = StatepointRecord::new(callsite.record).expect("decode statepoint record");
  assert!(
    sp.gc_pair_count() >= 1,
    "expected at least one (base,derived) pair"
  );

  let mut saw_distinct = false;
  for pair in sp.gc_pairs() {
    match (&pair.base, &pair.derived) {
      (
        Location::Indirect {
          offset: base_off, ..
        },
        Location::Indirect {
          offset: derived_off,
          ..
        },
      ) if base_off != derived_off => {
        saw_distinct = true;
        break;
      }
      _ => {}
    }
  }
  assert!(
    saw_distinct,
    "expected at least one derived pointer with a distinct spill slot"
  );
}

#[test]
fn stackmap_conformance_matrix() {
  if !cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
    return;
  }

  let Some(ld_lld) = find_cmd(&["ld.lld-18", "ld.lld"]) else {
    eprintln!("skipping stackmap_conformance_matrix: lld not found in PATH (need ld.lld-18 or ld.lld)");
    return;
  };
  let Some(objcopy) = find_cmd(&["llvm-objcopy-18", "llvm-objcopy", "objcopy"]) else {
    eprintln!("skipping stackmap_conformance_matrix: objcopy not found in PATH (need llvm-objcopy-18/llvm-objcopy/objcopy)");
    return;
  };

  // Keep this test optional in minimal environments (mirrors other runtime-native LLVM tests).
  let required = ["llvm-as-18", "opt-18", "llc-18"];
  if !required.iter().all(|c| has_cmd(c)) {
    eprintln!("skipping stackmap_conformance_matrix: LLVM 18 tools not found in PATH");
    return;
  }

  let dir = tempdir().expect("create tempdir");

  // Build two tiny statepoint modules:
  // - `simple`: one gc-live pointer (base==derived)
  // - `derived`: an interior pointer (base!=derived) to ensure base/derived pairs appear
  let simple_ll = dir.path().join("simple.ll");
  let simple_bc = dir.path().join("simple.bc");
  let simple_opt_bc = dir.path().join("simple.opt.bc");
  write_statepoint_module(&simple_ll, &statepoint_ir_simple("foo_simple"));
  compile_statepoint_module(&simple_ll, &simple_bc, &simple_opt_bc);

  let derived_ll = dir.path().join("derived.ll");
  let derived_bc = dir.path().join("derived.bc");
  let derived_opt_bc = dir.path().join("derived.opt.bc");
  write_statepoint_module(&derived_ll, &statepoint_ir_derived("foo_derived", "callee2"));
  compile_statepoint_module(&derived_ll, &derived_bc, &derived_opt_bc);

  for opt in ["-O0", "-O2", "-O3"] {
    let simple_obj = dir.path().join(format!("simple{opt}.o"));
    llc_to_obj(&simple_opt_bc, &simple_obj, opt);
    let simple_stackmaps_bin = dir.path().join(format!("simple{opt}.llvm_stackmaps.bin"));
    let bytes = dump_stackmaps(objcopy, &simple_obj, &simple_stackmaps_bin);
    let index = StackMaps::parse(&bytes).expect("parse stackmaps (simple)");
    validate_stackmaps(&index).expect("validate stackmaps (simple)");

    let derived_obj = dir.path().join(format!("derived{opt}.o"));
    llc_to_obj(&derived_opt_bc, &derived_obj, opt);
    let derived_stackmaps_bin = dir.path().join(format!("derived{opt}.llvm_stackmaps.bin"));
    let bytes = dump_stackmaps(objcopy, &derived_obj, &derived_stackmaps_bin);
    let index = StackMaps::parse(&bytes).expect("parse stackmaps (derived)");
    validate_stackmaps(&index).expect("validate stackmaps (derived)");
    assert_has_distinct_base_derived_pair(&index);
  }

  // Multi-object concatenation case:
  // Link two objects each containing `.llvm_stackmaps`, then ensure `StackMaps::parse` sees both
  // blobs and the merged index validates.
  let simple_obj = dir.path().join("simple_link.o");
  llc_to_obj(&simple_opt_bc, &simple_obj, "-O0");
  let derived_obj = dir.path().join("derived_link.o");
  llc_to_obj(&derived_opt_bc, &derived_obj, "-O0");

  let merged_obj = dir.path().join("merged.o");
  run(
    Command::new(ld_lld)
      .arg("-r")
      .arg("-o")
      .arg(&merged_obj)
      .arg(&simple_obj)
      .arg(&derived_obj),
  );
  let merged_stackmaps_bin = dir.path().join("merged.llvm_stackmaps.bin");
  let bytes = dump_stackmaps(objcopy, &merged_obj, &merged_stackmaps_bin);
  let merged = StackMaps::parse(&bytes).expect("parse merged stackmaps");
  assert_eq!(merged.raws().len(), 2, "expected two concatenated stackmap blobs");
  validate_stackmaps(&merged).expect("validate merged stackmaps");
}
