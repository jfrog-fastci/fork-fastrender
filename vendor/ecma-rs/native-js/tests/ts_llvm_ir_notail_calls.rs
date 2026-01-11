use native_js::{compile_typescript_to_llvm_ir, CompileOptions, EmitKind};

#[test]
fn ts_emitted_ir_marks_calls_notail() {
  // Ensure the TS→LLVM IR emitter marks calls as `notail`, so LLVM cannot turn
  // `call+ret` into a tailcall `jmp` and break stackmap-based stack walking.
  let mut opts = CompileOptions::default();
  opts.emit = EmitKind::LlvmIr;

  let ir = compile_typescript_to_llvm_ir(
    r#"
function callee(x: number): number { return x + 1; }
function caller(x: number): number { return callee(x); }
caller(1);
"#,
    opts,
  )
  .expect("compile TS to LLVM IR");

  assert!(ir.contains("notail call"), "IR missing notail call:\n{ir}");
}

