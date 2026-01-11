use std::io::Write;
use std::process::Command;

#[test]
fn llvm18_statepoint_rewrite_indirect_call_has_elementtype() {
  // In opaque-pointer mode (LLVM >= 15, and default in LLVM 18), the callee operand of
  // `llvm.experimental.gc.statepoint` must carry an `elementtype(<fn-ty>)`.
  //
  // This is especially important for *indirect calls* through a `ptr`-typed function pointer:
  // the call site's signature must be propagated to the statepoint's callee operand.
  let input_ir = r#"
; ModuleID = 'statepoints_indirect_call'
source_filename = "statepoints_indirect_call.ll"

declare void @callee(i64)

define void @test(ptr addrspace(1) %obj) gc "statepoint-example" {
entry:
  %fp_slot = alloca ptr, align 8
  store ptr @callee, ptr %fp_slot, align 8
  %fp = load ptr, ptr %fp_slot, align 8
  call void %fp(i64 123)
  %isnull = icmp eq ptr addrspace(1) %obj, null
  ret void
}
"#;

  let mut input_file = tempfile::NamedTempFile::new().expect("create temp IR file");
  input_file
    .write_all(input_ir.as_bytes())
    .expect("write temp IR file");

  let output = Command::new("opt-18")
    .args([
      "-passes=rewrite-statepoints-for-gc",
      "-S",
      input_file.path().to_str().expect("temp path is utf-8"),
      "-o",
      "-",
    ])
    .output()
    .expect("run opt-18 (is LLVM 18 installed and in PATH?)");

  if !output.status.success() {
    panic!(
      "opt-18 failed with status {}.\nstdout:\n{}\nstderr:\n{}",
      output.status,
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr),
    );
  }

  let rewritten = String::from_utf8_lossy(&output.stdout);

  // Statepoint inserted.
  assert!(
    rewritten.contains("llvm.experimental.gc.statepoint.p0"),
    "expected gc.statepoint intrinsic in rewritten IR, got:\n{rewritten}"
  );

  // Indirect call's callee operand must carry elementtype(void (i64)).
  assert!(
    rewritten.contains("ptr elementtype(void (i64)) %fp"),
    "expected statepoint callee operand to be `ptr elementtype(void (i64)) %fp`, got:\n{rewritten}"
  );

  // %obj is live across the call => it must be in the gc-live bundle.
  assert!(
    rewritten.contains("\"gc-live\"(ptr addrspace(1) %obj)"),
    "expected `\"gc-live\"(ptr addrspace(1) %obj)` operand bundle, got:\n{rewritten}"
  );

  // ...and thus a relocate for %obj must exist.
  assert!(
    rewritten.contains("%obj.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1"),
    "expected gc.relocate for %obj in rewritten IR, got:\n{rewritten}"
  );
}
