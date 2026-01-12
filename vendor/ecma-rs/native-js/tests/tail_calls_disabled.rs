use std::fs;

use anyhow::Result;
use tempfile::tempdir;

use native_js::toolchain::{LlvmToolchain, OptLevel};
use native_js::ts_ir::{
  tailcall_regression_module_ir, TAILCALL_TEST_CALLEE, TAILCALL_TEST_CALLER, TAILCALL_TEST_INDIRECT_CALLER,
};
use native_js::tail_calls::{
  assert_function_calls_symbol, assert_function_does_not_jump_to_symbol, assert_function_ends_with_ret,
  assert_function_has_call, assert_function_has_ret, assert_no_tail_call_jumps, assert_objdump_has_section,
};

#[test]
fn ts_codegen_disables_tail_calls_in_optimized_builds() -> Result<()> {
  let tc = match LlvmToolchain::detect() {
    Ok(tc) => tc,
    Err(_) => {
      eprintln!("skipping: LLVM toolchain not found in PATH (need clang + llvm-objdump)");
      return Ok(());
    }
  };
  if tc.llvm_objdump.is_none() {
    eprintln!("skipping: llvm-objdump not found in PATH (need llvm-objdump-18 or llvm-objdump)");
    return Ok(());
  }
  let triple = tc.host_target_triple()?;
  let ir = tailcall_regression_module_ir(&triple);

  // IR-level invariants: required by design, and checked here so this test remains useful
  // even if LLVM heuristics change.
  assert!(
    ir.contains("\"disable-tail-calls\"=\"true\""),
    "expected TS functions to have disable-tail-calls attribute"
  );
  assert!(
    ir.contains("notail call"),
    "expected tail-position calls to be emitted as notail"
  );
  assert!(
    ir.contains("notail call i64 %fp"),
    "expected indirect tail-position call to be emitted as notail:\n{ir}"
  );

  let tmp = tempdir()?;
  let ll_path = tmp.path().join("module.ll");
  let obj_path = tmp.path().join("module.o");

  fs::write(&ll_path, ir)?;
  tc.compile_ll_to_object(&ll_path, &obj_path, OptLevel::O3)?;

  // Ensure we actually emitted stackmaps (mirrors our planned statepoint-based stack walking).
  let sections = tc.objdump_section_headers(&obj_path)?;
  assert_objdump_has_section(&sections, ".llvm_stackmaps")?;

  let disasm = tc.objdump_disassemble_with_relocs(&obj_path)?;

  // General invariant: TS functions must not contain tailcall-style jumps.
  assert_no_tail_call_jumps(
    &disasm,
    &[
      TAILCALL_TEST_CALLER,
      TAILCALL_TEST_CALLEE,
      TAILCALL_TEST_INDIRECT_CALLER,
    ],
  )?;

  // Regression-specific: a tail-position call must remain `call` + `ret` (not `jmp`).
  assert_function_calls_symbol(&disasm, TAILCALL_TEST_CALLER, TAILCALL_TEST_CALLEE)?;
  assert_function_does_not_jump_to_symbol(&disasm, TAILCALL_TEST_CALLER, TAILCALL_TEST_CALLEE)?;
  assert_function_has_ret(&disasm, TAILCALL_TEST_CALLER)?;

  // Indirect tailcall regression: without `notail`/disable-tail-calls this would compile to `jmp *%reg`.
  assert_function_has_call(&disasm, TAILCALL_TEST_INDIRECT_CALLER)?;
  assert_function_has_ret(&disasm, TAILCALL_TEST_INDIRECT_CALLER)?;
  assert_function_ends_with_ret(&disasm, TAILCALL_TEST_INDIRECT_CALLER)?;

  Ok(())
}
