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
fn compiled_script_rejects_await_in_class_static_block_even_with_async_classic_script_retry() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  // `vm-js` retries classic scripts with top-level await enabled ("async classic scripts"), but
  // class static blocks do not inherit that async context. `await` in class static blocks is
  // therefore still a syntax error in scripts.
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
  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_script_with_budget_rejects_await_in_class_static_block_even_with_async_classic_script_retry(
) {
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
  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_script_rejects_super_property_in_top_level_arrow_function() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  // Mirrors test262 `language/global-code/super-prop-arrow.js`.
  let err = CompiledScript::compile_script(&mut heap, "test.js", "() => { super.property; };")
    .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_script_rejects_super_property_in_async_arrow_function() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  // Mirrors test262 `language/expressions/async-arrow-function/early-errors-arrow-body-contains-super-property.js`.
  let err =
    CompiledScript::compile_script(&mut heap, "test.js", "async(foo) => { super.prop };")
      .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_script_rejects_super_property_in_async_arrow_function_formals() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  // Mirrors test262 `language/expressions/async-arrow-function/early-errors-arrow-formals-contains-super-property.js`.
  let err =
    CompiledScript::compile_script(&mut heap, "test.js", "async (foo = super.foo) => { }")
      .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_script_rejects_super_property_in_async_function_expression_formals() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  // Mirrors test262 `language/expressions/async-function/early-errors-expression-formals-contains-super-property.js`.
  let err = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    "(async function foo (foo = super.foo) { var bar; });",
  )
  .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_script_rejects_super_call_at_script_top_level() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  // Mirrors test262 `language/global-code/super-call.js`.
  let err = CompiledScript::compile_script(&mut heap, "test.js", "super();").unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_script_rejects_super_call_in_top_level_arrow_function() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  // Mirrors test262 `language/global-code/super-call-arrow.js`.
  let err = CompiledScript::compile_script(&mut heap, "test.js", "() => { super(); };").unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_script_rejects_super_call_in_async_arrow_function() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  // Mirrors test262 `language/expressions/async-arrow-function/early-errors-arrow-body-contains-super-call.js`.
  let err =
    CompiledScript::compile_script(&mut heap, "test.js", "async(foo) => { super(); };")
      .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_script_rejects_super_call_in_class_field_initializer() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let err = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    "class B {} class A extends B { x = super(); }",
  )
  .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_script_rejects_super_call_in_static_field_initializer() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let err = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    "class B {} class A extends B { static x = super(); }",
  )
  .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn compiled_script_rejects_super_call_in_class_static_block() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let err = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    "class B {} class A extends B { static { super(); } }",
  )
  .unwrap_err();

  match err {
    VmError::Syntax(diags) => assert!(!diags.is_empty()),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}
