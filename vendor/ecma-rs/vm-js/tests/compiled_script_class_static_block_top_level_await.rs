use vm_js::{CompiledScript, Heap, HeapLimits, VmError};

#[test]
fn compiled_script_rejects_await_in_class_expression_static_block() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  // `vm-js` retries classic scripts with top-level `await` enabled ("async classic scripts"), so
  // parsing can succeed here. However, `await` expressions are still an early error in class static
  // blocks, so compilation must fail with a syntax error.
  //
  // Use a class *expression* (not a declaration) to ensure early errors are still detected when
  // the class appears in expression position.
  let err = CompiledScript::compile_script(
    &mut heap,
    "script.js",
    r#"
      (class {
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
