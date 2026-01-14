use vm_js::{
  CompiledFunctionRef, CompiledScript, Heap, HeapLimits, JsRuntime, MicrotaskQueue, PropertyKey,
  PromiseState, SourceText, SourceTextModuleRecord, Value, Vm, VmError, VmOptions,
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

#[test]
fn compiled_async_block_body_await_expr_stmt_implicit_throw_has_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Promise/async-await allocates job machinery; use a larger heap to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "const _ = async () => { await (null).x; };",
  )?;

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
      if matches!(meta.body, hir_js::FunctionBody::Block(_)) {
        found = Some(*body_id);
        break;
      }
    }
    found.ok_or(VmError::InvariantViolation(
      "async arrow function block body not found in compiled script",
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
    r#"typeof captured === "string" && captured.includes("TypeError") && captured.includes("at ") && captured.includes("test.js:1:")"#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_async_block_body_await_var_decl_throw_after_resume_has_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Promise/async-await allocates job machinery; use a larger heap to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "const _ = async () => { const { x } = await Promise.resolve(null); };",
  )?;

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
      if matches!(meta.body, hir_js::FunctionBody::Block(_)) {
        found = Some(*body_id);
        break;
      }
    }
    found.ok_or(VmError::InvariantViolation(
      "async arrow function block body not found in compiled script",
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
    r#"typeof captured === "string" && captured.includes("TypeError") && captured.includes("at ") && captured.includes("test.js:1:")"#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_async_block_body_throw_await_attaches_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Promise/async-await allocates job machinery; use a larger heap to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // `new Error()` does not capture/attach a stack by itself in vm-js; it is attached when the value
  // is thrown. This exercises `throw await <expr>;` resumption which must still attach `stack`.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "const _ = async () => { throw await Promise.resolve(new Error(\"boom\")); };",
  )?;

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
      if matches!(meta.body, hir_js::FunctionBody::Block(_)) {
        found = Some(*body_id);
        break;
      }
    }
    found.ok_or(VmError::InvariantViolation(
      "async arrow function block body not found in compiled script",
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
    r#"typeof captured === "string" && captured.includes("boom") && captured.includes("at ") && captured.includes("test.js:1:")"#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_async_block_body_await_revoked_proxy_promise_resolve_throw_has_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Promise/async-await allocates job machinery; use a larger heap to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Awaiting a revoked Proxy throws during the `Await` abstract op's internal `PromiseResolve` step.
  // This happens in the async suspension machinery (not in the statement evaluator), so it's a
  // regression test that `Error.stack` is still attached and attributed to the await site.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "const _ = async () => {\n  const { proxy, revoke } = Proxy.revocable({}, {});\n  revoke();\n  await proxy;\n};",
  )?;

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
      if matches!(meta.body, hir_js::FunctionBody::Block(_)) {
        found = Some(*body_id);
        break;
      }
    }
    found.ok_or(VmError::InvariantViolation(
      "async arrow function block body not found in compiled script",
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
    r#"typeof captured === "string" && captured.includes("TypeError") && captured.includes("revoked Proxy") && captured.includes("at ") && captured.includes("test.js:4:3")"#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_top_level_await_implicit_throw_has_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Top-level await allocates Promise/job machinery; use a larger heap to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Use `await null.x` (rather than `await (null).x`) so script-mode parsing cannot treat `await`
  // as an identifier call expression; this ensures the compiler uses the module grammar fallback
  // and the script executes via async classic script evaluation.
  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", "await null.x;")?;

  let completion = rt.exec_compiled_script(script)?;
  let completion_root = rt.heap_mut().add_root(completion)?;

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object, got {completion:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);

  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a result");
  let Value::Object(reason_obj) = reason else {
    panic!("expected rejected promise reason to be an object, got {reason:?}");
  };

  let stack = {
    let mut scope = rt.heap_mut().scope();
    scope.push_root(Value::Object(reason_obj))?;

    let stack_key_s = scope.alloc_string("stack")?;
    scope.push_root(Value::String(stack_key_s))?;
    let stack_key = PropertyKey::from_string(stack_key_s);

    let stack_v = scope
      .heap()
      .object_get_own_data_property_value(reason_obj, &stack_key)?
      .unwrap_or(Value::Undefined);
    let Value::String(stack_s) = stack_v else {
      panic!("expected stack string, got {stack_v:?}");
    };
    scope.heap().get_string(stack_s)?.to_utf8_lossy()
  };

  assert!(!stack.is_empty(), "expected non-empty stack string");
  assert!(
    stack.contains("TypeError"),
    "expected stack string to contain error name, got {stack:?}"
  );
  assert!(
    stack.contains("at ") && stack.contains("test.js:1:1"),
    "expected stack string to contain stack frames, got {stack:?}"
  );

  rt.heap_mut().remove_root(completion_root);

  Ok(())
}

#[test]
fn compiled_top_level_await_revoked_proxy_promise_resolve_throw_has_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Promise/job machinery needs a bit of heap headroom.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "const { proxy, revoke } = Proxy.revocable({}, {});\nrevoke();\nawait proxy;",
  )?;

  let completion = rt.exec_compiled_script(script)?;
  let completion_root = rt.heap_mut().add_root(completion)?;

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object, got {completion:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a result");
  let Value::Object(reason_obj) = reason else {
    panic!("expected rejected promise reason to be an object, got {reason:?}");
  };

  let stack = {
    let mut scope = rt.heap_mut().scope();
    scope.push_root(Value::Object(reason_obj))?;

    let stack_key_s = scope.alloc_string("stack")?;
    scope.push_root(Value::String(stack_key_s))?;
    let stack_key = PropertyKey::from_string(stack_key_s);

    let stack_v = scope
      .heap()
      .object_get_own_data_property_value(reason_obj, &stack_key)?
      .unwrap_or(Value::Undefined);
    let Value::String(stack_s) = stack_v else {
      panic!("expected stack string, got {stack_v:?}");
    };
    scope.heap().get_string(stack_s)?.to_utf8_lossy()
  };

  assert!(!stack.is_empty(), "expected non-empty stack string");
  assert!(
    stack.contains("TypeError") && stack.contains("revoked Proxy"),
    "expected stack string to contain error name/message, got {stack:?}"
  );
  assert!(
    stack.contains("at ") && stack.contains("test.js:3:1"),
    "expected stack string to contain stack frames, got {stack:?}"
  );

  rt.heap_mut().remove_root(completion_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_rejection_reason_has_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Promise + microtask machinery needs a bit of heap headroom.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      const notCallable = 1;
      const x = notCallable();
      await Promise.resolve(0);
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let promise_root = rt.heap.add_root(result)?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
    panic!("expected Promise object from top-level await script");
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Rejected);

  let reason = rt
    .heap
    .promise_result(promise_obj)?
    .expect("rejected Promise should have a rejection reason");
  let Value::Object(err_obj) = reason else {
    panic!("expected Promise rejection reason to be an object, got {reason:?}");
  };

  // `Error.stack` must be attached on the compiled/HIR path even for top-level await scripts that
  // reject via a synchronous throw before the first `await`.
  let stack = {
    let mut scope = rt.heap.scope();
    let stack_key_s = scope.alloc_string("stack")?;
    scope.push_root(Value::String(stack_key_s))?;
    let stack_key = PropertyKey::from_string(stack_key_s);
    let stack_value = scope
      .heap()
      .object_get_own_data_property_value(err_obj, &stack_key)?
      .unwrap_or(Value::Undefined);

    let Value::String(stack_s) = stack_value else {
      panic!("expected rejection reason to have string own `stack`, got {stack_value:?}");
    };
    scope.heap().get_string(stack_s)?.to_utf8_lossy()
  };
  assert!(!stack.is_empty(), "expected non-empty stack string");
  assert!(
    stack.contains("value is not callable"),
    "expected stack string to include error message, got {stack:?}"
  );

  rt.heap_mut().remove_root(promise_root);
  Ok(())
}

#[test]
fn compiled_top_level_await_destructure_throw_after_resume_has_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Top-level await allocates Promise/job machinery; use a larger heap to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Destructuring happens *after* the await resumes, so this exercises async-script resume logic.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "const { x } = await Promise.resolve(null);",
  )?;

  let completion = rt.exec_compiled_script(script)?;
  let completion_root = rt.heap_mut().add_root(completion)?;

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object, got {completion:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a result");
  let Value::Object(reason_obj) = reason else {
    panic!("expected rejected promise reason to be an object, got {reason:?}");
  };

  let stack = {
    let mut scope = rt.heap_mut().scope();
    scope.push_root(Value::Object(reason_obj))?;

    let stack_key_s = scope.alloc_string("stack")?;
    scope.push_root(Value::String(stack_key_s))?;
    let stack_key = PropertyKey::from_string(stack_key_s);

    let stack_v = scope
      .heap()
      .object_get_own_data_property_value(reason_obj, &stack_key)?
      .unwrap_or(Value::Undefined);
    let Value::String(stack_s) = stack_v else {
      panic!("expected stack string, got {stack_v:?}");
    };
    scope.heap().get_string(stack_s)?.to_utf8_lossy()
  };

  assert!(!stack.is_empty(), "expected non-empty stack string");
  assert!(
    stack.contains("TypeError"),
    "expected stack string to contain error name, got {stack:?}"
  );
  assert!(
    stack.contains("at ") && stack.contains("test.js:1:1"),
    "expected stack string to contain stack frames, got {stack:?}"
  );

  rt.heap_mut().remove_root(completion_root);
  Ok(())
}

#[test]
fn compiled_top_level_await_assignment_throw_after_resume_has_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Top-level await allocates Promise/job machinery; use a larger heap to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Unresolvable assignment in strict mode throws on PutValue, which happens *after* the await
  // resumes.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "\"use strict\";\nx = await Promise.resolve(0);",
  )?;

  let completion = rt.exec_compiled_script(script)?;
  let completion_root = rt.heap_mut().add_root(completion)?;

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object, got {completion:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a result");
  let Value::Object(reason_obj) = reason else {
    panic!("expected rejected promise reason to be an object, got {reason:?}");
  };

  let stack = {
    let mut scope = rt.heap_mut().scope();
    scope.push_root(Value::Object(reason_obj))?;

    let stack_key_s = scope.alloc_string("stack")?;
    scope.push_root(Value::String(stack_key_s))?;
    let stack_key = PropertyKey::from_string(stack_key_s);

    let stack_v = scope
      .heap()
      .object_get_own_data_property_value(reason_obj, &stack_key)?
      .unwrap_or(Value::Undefined);
    let Value::String(stack_s) = stack_v else {
      panic!("expected stack string, got {stack_v:?}");
    };
    scope.heap().get_string(stack_s)?.to_utf8_lossy()
  };

  assert!(!stack.is_empty(), "expected non-empty stack string");
  assert!(
    stack.contains("ReferenceError"),
    "expected stack string to contain error name, got {stack:?}"
  );
  assert!(
    stack.contains("at ") && stack.contains("test.js:2:1"),
    "expected stack string to contain stack frames, got {stack:?}"
  );

  rt.heap_mut().remove_root(completion_root);
  Ok(())
}
