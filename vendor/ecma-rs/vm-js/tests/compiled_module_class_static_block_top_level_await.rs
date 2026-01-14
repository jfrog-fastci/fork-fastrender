use vm_js::{CompiledScript, Heap, HeapLimits, VmError};

#[test]
fn compiled_module_rejects_await_in_class_expression_static_block() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  // `await` expressions are an early error in class static blocks. Modules always parse with
  // `await` enabled, but class static blocks are still `~Await`, so compilation must fail with a
  // syntax error.
  let err = CompiledScript::compile_module(
    &mut heap,
    "m.js",
    r#"
      export default (class {
        static {
          await Promise.resolve(0);
        }
      });
    "#,
  )
  .unwrap_err();
  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }

  Ok(())
}
