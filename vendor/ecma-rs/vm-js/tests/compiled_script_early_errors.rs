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
fn compiled_script_rejects_await_in_class_static_block() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let err = CompiledScript::compile_script(
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
  .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn compiled_script_with_budget_rejects_await_in_class_static_block() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());

  let err = CompiledScript::compile_script_with_budget(
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
  .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}
