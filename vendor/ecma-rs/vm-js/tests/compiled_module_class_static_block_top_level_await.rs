use vm_js::{CompiledScript, Heap, HeapLimits, VmError};

#[test]
fn compiled_module_detects_top_level_await_in_class_expression_static_block() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  // Class static blocks execute during class evaluation, so `await` inside a static block at
  // module top-level should be treated as top-level await (requiring async evaluation).
  //
  // Use a class *expression* (not a declaration) so the detector must descend through
  // `expr_contains_await`.
  let script = CompiledScript::compile_module(
    &mut heap,
    "m.js",
    r#"
      export default (class {
        static {
          await Promise.resolve(0);
        }
      });
    "#,
  )?;

  assert!(
    script.contains_top_level_await,
    "await in class static blocks should set contains_top_level_await for compiled modules"
  );
  assert!(
    script.top_level_await_requires_ast_fallback,
    "await inside class static blocks is not supported by the HIR async executor; it must fall back to the AST evaluator"
  );

  Ok(())
}

