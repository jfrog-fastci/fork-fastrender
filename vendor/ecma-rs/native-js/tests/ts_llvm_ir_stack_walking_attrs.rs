use native_js::{compile_typescript_to_llvm_ir, CompileOptions, EmitKind};

#[test]
fn ts_emitted_ir_has_stack_walking_attributes() {
  // The parse-js driven emitter always produces a `main` function.
  //
  // Use an empty program so we don't depend on statement support here; this test is only checking
  // codegen-level invariants.
  let mut opts = CompileOptions::default();
  opts.emit = EmitKind::LlvmIr;
  let ir = compile_typescript_to_llvm_ir(
    "",
    opts,
  )
  .expect("compile TS to LLVM IR");

  assert!(
    ir.contains("\"frame-pointer\"=\"all\""),
    "IR missing frame-pointer attribute:\n{ir}"
  );
  assert!(
    ir.contains("\"disable-tail-calls\"=\"true\"") || ir.contains("disable-tail-calls"),
    "IR missing disable-tail-calls attribute:\n{ir}"
  );
}
