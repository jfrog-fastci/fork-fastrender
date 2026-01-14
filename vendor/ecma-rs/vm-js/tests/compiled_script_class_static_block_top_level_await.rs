use vm_js::{CompiledScript, Heap, HeapLimits, VmError};

#[test]
fn compiled_script_detects_top_level_await_in_class_expression_static_block() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  // Class static blocks execute during class evaluation, so `await` inside a static block at
  // script top-level (async classic scripts) must be treated as top-level await (requiring async
  // evaluation).
  //
  // Use a class *expression* so the detector must descend through `expr_contains_await`.
  let script = CompiledScript::compile_script(
    &mut heap,
    "script.js",
    r#"
      (class {
        static {
          await Promise.resolve(0);
        }
      });
    "#,
  )?;

  assert!(
    script.contains_top_level_await,
    "await in class static blocks should set contains_top_level_await for compiled scripts"
  );
  assert!(
    script.top_level_await_requires_ast_fallback,
    "await inside class static blocks is not supported by the HIR async classic-script executor; it must fall back to the AST evaluator"
  );
  assert!(
    script.requires_ast_fallback,
    "unsupported top-level await patterns should set `requires_ast_fallback` for compiled-script execution"
  );

  Ok(())
}

