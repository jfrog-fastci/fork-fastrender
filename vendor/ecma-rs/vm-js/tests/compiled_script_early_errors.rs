use vm_js::{CompiledScript, Heap, HeapLimits, Vm, VmError, VmOptions};

#[test]
fn compiled_script_rejects_invalid_private_name() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let err = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      class C {
        m() {
          return this.#x;
        }
      }
    "#,
  )
  .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_script_with_budget_rejects_invalid_private_name() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let err = CompiledScript::compile_script_with_budget(
    &mut heap,
    &mut vm,
    "test.js",
    r#"
      class C {
        m() {
          return this.#x;
        }
      }
    "#,
  )
  .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_module_rejects_invalid_private_name() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let err = CompiledScript::compile_module(
    &mut heap,
    "test.js",
    r#"
      class C {
        m() {
          return this.#x;
        }
      }
    "#,
  )
  .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_module_with_budget_rejects_invalid_private_name() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let err = CompiledScript::compile_module_with_budget(
    &mut heap,
    &mut vm,
    "test.js",
    r#"
      class C {
        m() {
          return this.#x;
        }
      }
    "#,
  )
  .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_module_rejects_exporting_unresolvable_binding() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let err = CompiledScript::compile_module(&mut heap, "test.js", r#"export { unresolvable };"#)
    .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_module_with_budget_rejects_exporting_unresolvable_binding() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let err = CompiledScript::compile_module_with_budget(
    &mut heap,
    &mut vm,
    "test.js",
    r#"export { unresolvable };"#,
  )
  .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_module_rejects_duplicate_exported_name_default_vs_named() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let err = CompiledScript::compile_module(
    &mut heap,
    "test.js",
    r#"
      var x;
      export default 1;
      export { x as default };
    "#,
  )
  .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_module_with_budget_rejects_duplicate_exported_name_default_vs_named() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let err = CompiledScript::compile_module_with_budget(
    &mut heap,
    &mut vm,
    "test.js",
    r#"
      var x;
      export default 1;
      export { x as default };
    "#,
  )
  .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_script_allows_await_in_class_static_block_via_async_classic_script_retry() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      class C {
        static {
          await Promise.resolve(0);
        }
      }
    "#,
  )
  .unwrap();

  assert!(
    script.contains_top_level_await,
    "await in class static blocks should be treated as top-level await for async classic scripts"
  );
  assert!(
    script.top_level_await_requires_ast_fallback,
    "await in class static blocks is not supported by the HIR async script executor; compilation should request AST fallback"
  );
}

#[test]
fn compiled_script_with_budget_allows_await_in_class_static_block_via_async_classic_script_retry() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());

  let script = CompiledScript::compile_script_with_budget(
    &mut heap,
    &mut vm,
    "test.js",
    r#"
      class C {
        static {
          await Promise.resolve(0);
        }
      }
    "#,
  )
  .unwrap();
  assert!(script.contains_top_level_await);
  assert!(script.top_level_await_requires_ast_fallback);
}
