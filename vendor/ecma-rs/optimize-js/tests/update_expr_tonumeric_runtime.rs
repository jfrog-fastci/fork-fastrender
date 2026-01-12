#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use emit_js::EmitOptions;
use optimize_js::{decompile::program_to_js, DecompileOptions, TopLevelMode};
use vm_js::{Heap, HeapLimits, JsBigInt, JsRuntime, Value, Vm, VmOptions};

#[test]
fn update_expr_to_numeric_handles_object_returning_bigint() {
  let src = r#"
    let x = { valueOf: function () { return 1n; } };
    x++;
    globalThis.__out = x;
  "#;

  let program = compile_source(src, TopLevelMode::Global, false);
  // Predeclare temporaries so the structured output remains runnable even when the decompiler
  // falls back to a state machine (irreducible control flow).
  let opts = DecompileOptions {
    declare_registers: true,
    ..DecompileOptions::default()
  };
  let bytes = program_to_js(&program, &opts, EmitOptions::minified())
    .expect("emit optimized JS");
  let js = std::str::from_utf8(&bytes).expect("UTF-8 output");

  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap).expect("create vm-js runtime");
  rt
    .exec_script(js)
    .unwrap_or_else(|err| panic!("execute optimized JS: {err:?}\n\n{js}"));

  let out = rt.exec_script("globalThis.__out").expect("read output");
  assert_eq!(out, Value::BigInt(JsBigInt::from_i128(2)));
}
