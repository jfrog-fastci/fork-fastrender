use inkwell::context::Context;
use native_js::CodeGen;

#[test]
fn emitted_ir_has_stack_walking_attributes() {
  let context = Context::create();
  let cg = CodeGen::new(&context, "test");
  cg.define_trivial_function("trivial");

  let ir = cg.module_ir();

  assert!(
    ir.contains("\"frame-pointer\"=\"all\""),
    "IR missing frame-pointer attribute:\n{ir}"
  );

  // LLVM prints `disable-tail-calls` as a string attribute in the function attribute group.
  assert!(
    ir.contains("\"disable-tail-calls\"=\"true\"") || ir.contains("disable-tail-calls"),
    "IR missing disable-tail-calls attribute:\n{ir}"
  );
}

