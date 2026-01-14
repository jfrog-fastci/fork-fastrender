use vm_js::{
  CompiledFunctionRef, CompiledScript, Heap, HeapLimits, JsRuntime, MicrotaskQueue, PropertyKey,
  SourceText, SourceTextModuleRecord, Value, Vm, VmError, VmOptions,
};

#[test]
fn compiled_script_catch_has_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      try {
        throw new Error("boom");
      } catch (e) {
        e.stack;
      }
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;

  let Value::String(stack_s) = result else {
    panic!("expected script to return stack string, got {result:?}");
  };
  let stack = rt.heap().get_string(stack_s)?.to_utf8_lossy();
  assert!(!stack.is_empty(), "expected non-empty stack string");
  assert!(
    stack.contains("boom"),
    "expected stack string to contain error message, got {stack:?}"
  );
  Ok(())
}

#[test]
fn compiled_module_catch_has_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Store the captured stack on the global object so we can assert on it after module evaluation.
  // This avoids needing to plumb module namespace exports into the test.
  // Avoid `Arc::new`, which can abort the process on allocator OOM.
  let source = SourceText::new_charged_arc(
    rt.heap_mut(),
    "m.js",
    r#"
      globalThis.__stack = (function () {
        try {
          throw new Error("boom");
        } catch (e) {
          return e.stack;
        }
      })();

      export {};
    "#,
  )?;

  let record = SourceTextModuleRecord::compile_source(rt.heap_mut(), source)?;
  let module_id = rt.modules_mut().add_module(record)?;

  let global_object = rt.realm().global_object();
  let realm_id = rt.realm().id();

  // Evaluate the module via the module graph so it executes through the compiled (HIR) executor
  // when `compiled` is present.
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();
  {
    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    modules.evaluate_sync(vm, heap, global_object, realm_id, module_id, &mut host, &mut hooks)?;
  }

  let result = rt.exec_script("globalThis.__stack")?;
  let Value::String(stack_s) = result else {
    panic!("expected module to set stack string, got {result:?}");
  };
  let stack = rt.heap().get_string(stack_s)?.to_utf8_lossy();
  assert!(!stack.is_empty(), "expected non-empty stack string");
  assert!(
    stack.contains("boom"),
    "expected stack string to contain error message, got {stack:?}"
  );
  Ok(())
}

#[test]
fn compiled_async_expr_body_implicit_throw_has_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Promise/async-await allocates job machinery; use a larger heap to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(rt.heap_mut(), "<inline>", "const _ = async () => (null).x;")?;

  // Find the async arrow function body so we can invoke it via `CallHandler::User` (compiled path).
  let func_body = {
    let hir = script.hir.as_ref();
    let mut found: Option<hir_js::BodyId> = None;
    for (body_id, idx) in hir.body_index.iter() {
      let body = hir
        .bodies
        .get(*idx)
        .ok_or(VmError::InvariantViolation("hir body index out of bounds"))?;
      if body.kind != hir_js::BodyKind::Function {
        continue;
      }
      let Some(meta) = body.function.as_ref() else {
        continue;
      };
      if !meta.async_ || !meta.is_arrow {
        continue;
      }
      if matches!(meta.body, hir_js::FunctionBody::Expr(_)) {
        found = Some(*body_id);
        break;
      }
    }
    found.ok_or(VmError::InvariantViolation(
      "async arrow function body not found in compiled script",
    ))?
  };

  // Define the compiled function on the global object so we can call it from JS.
  {
    let global = rt.realm().global_object();
    let mut scope = rt.heap_mut().scope();
    scope.push_root(Value::Object(global))?;

    let name_s = scope.alloc_string("f")?;
    scope.push_root(Value::String(name_s))?;

    let f_obj = scope.alloc_user_function(
      CompiledFunctionRef {
        script,
        body: func_body,
      },
      name_s,
      0,
    )?;
    scope.push_root(Value::Object(f_obj))?;

    let key_s = scope.alloc_string("f")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    scope.define_property(
      global,
      key,
      vm_js::PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: vm_js::PropertyKind::Data {
          value: Value::Object(f_obj),
          writable: true,
        },
      },
    )?;
  }

  rt.exec_script(
    r#"
      var captured = "";
      f().catch(e => { captured = e.stack; });
    "#,
  )?;
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let v = rt.exec_script(
    r#"typeof captured === "string" && captured.includes("TypeError") && captured.includes("at ")"#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}
