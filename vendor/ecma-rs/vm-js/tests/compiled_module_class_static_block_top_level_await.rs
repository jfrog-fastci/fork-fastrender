use vm_js::{CompiledScript, Heap, HeapLimits, VmError};

#[test]
fn compiled_module_rejects_await_in_class_expression_static_block() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  // Class static blocks are parsed in a `~Await` context, so `await` is a syntax error even in
  // modules (top-level await).
  //
  // Use a class *expression* (not a declaration) so we cover that path as well.
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
