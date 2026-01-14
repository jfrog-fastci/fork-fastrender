use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

#[test]
fn compiled_hir_class_static_block_hoists_function_decls_and_does_not_leak() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Ensure class static blocks are executed on the compiled-HIR path and that function declarations
  // inside the block are instantiated before evaluation (hoisted) without leaking outwards.
  //
  // We include an async function declaration to ensure this path does *not* enable Annex B
  // block-function hoisting semantics (which deliberately skip async/generator decls).
  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "<inline>",
    r#"
      var inBlock = "";
      var inAsync = "";
      var outBlock = "";
      var outAsync = "";
      class C {
        static {
          inBlock = typeof f;
          inAsync = typeof af;
          function f() {}
          async function af() {}
        }
      }
      outBlock = typeof f;
      outAsync = typeof af;
      inBlock === "function" && inAsync === "function" && outBlock === "undefined" && outAsync === "undefined";
    "#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  assert!(matches!(result, Value::Bool(true)), "unexpected result: {result:?}");
  Ok(())
}
