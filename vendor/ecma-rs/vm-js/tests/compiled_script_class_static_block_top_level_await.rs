use vm_js::{CompiledScript, Heap, HeapLimits, VmError};

#[test]
fn compiled_script_rejects_await_in_class_expression_static_block() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  // `vm-js` retries classic scripts with top-level await enabled ("async classic scripts"), but
  // class static blocks are parsed in a `~Await` context, so `await` is still a syntax error.
  //
  // Use a class *expression* so we cover that path as well as the class-declaration case.
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
