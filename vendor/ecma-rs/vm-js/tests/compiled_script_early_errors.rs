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
  // `parse-js` intentionally does **not** allow class static blocks to inherit that async context.
  // `await` in class static blocks is therefore still a syntax error in scripts.
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
fn compiled_script_with_budget_rejects_await_in_class_static_block_even_with_async_classic_script_retry() {
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
