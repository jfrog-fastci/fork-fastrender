use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

#[test]
fn compiled_hir_class_static_block_hoists_function_decls_and_does_not_leak() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Ensure class static blocks are executed on the compiled-HIR path and that function declarations
  // inside the block are instantiated before evaluation (hoisted) without leaking outwards.
  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "<inline>",
    r#"
      var inBlock = "";
      var outBlock = "";
      class C {
        static {
          inBlock = typeof f;
          function f() {}
        }
      }
      outBlock = typeof f;
      inBlock === "function" && outBlock === "undefined";
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert!(matches!(result, Value::Bool(true)), "unexpected result: {result:?}");
  Ok(())
}

