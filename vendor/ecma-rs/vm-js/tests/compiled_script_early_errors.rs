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

