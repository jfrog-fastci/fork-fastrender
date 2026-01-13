use std::sync::Arc;
use vm_js::{
  Budget, CompiledFunctionRef, CompiledScript, Heap, HeapLimits, JsRuntime, TerminationReason,
  Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

fn find_function_body(script: &Arc<CompiledScript>, name: &str) -> hir_js::BodyId {
  let hir = script.hir.as_ref();
  for def in hir.defs.iter() {
    let Some(body_id) = def.body else {
      continue;
    };
    let Some(body) = hir.body(body_id) else {
      continue;
    };
    if body.kind != hir_js::BodyKind::Function {
      continue;
    }
    let def_name = hir.names.resolve(def.name).unwrap_or("");
    if def_name == name {
      return body_id;
    }
  }
  panic!("function body not found for name={name:?}");
}

#[test]
fn compiled_closure_capture_semantics() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function makeAdder(x) {
        return function(y) { return x + y; };
      }
    "#,
  )?;
  let make_adder_body = find_function_body(&script, "makeAdder");
  let mut vm = Vm::new(VmOptions::default());

  let mut scope = heap.scope();
  let name = scope.alloc_string("makeAdder")?;
  let make_adder = scope.alloc_user_function(
    CompiledFunctionRef {
      script: script.clone(),
      body: make_adder_body,
    },
    name,
    1,
  )?;

  // makeAdder(2)(3) == 5
  let closure = vm.call_without_host(
    &mut scope,
    Value::Object(make_adder),
    Value::Undefined,
    &[Value::Number(2.0)],
  )?;
  let result = vm.call_without_host(
    &mut scope,
    closure,
    Value::Undefined,
    &[Value::Number(3.0)],
  )?;
  assert_eq!(result, Value::Number(5.0));

  // Ensure closures capture independently.
  let add10 = vm.call_without_host(
    &mut scope,
    Value::Object(make_adder),
    Value::Undefined,
    &[Value::Number(10.0)],
  )?;
  let add20 = vm.call_without_host(
    &mut scope,
    Value::Object(make_adder),
    Value::Undefined,
    &[Value::Number(20.0)],
  )?;

  let r10 = vm.call_without_host(&mut scope, add10, Value::Undefined, &[Value::Number(1.0)])?;
  let r20 = vm.call_without_host(&mut scope, add20, Value::Undefined, &[Value::Number(1.0)])?;
  assert_eq!(r10, Value::Number(11.0));
  assert_eq!(r20, Value::Number(21.0));

  Ok(())
}

#[test]
fn compiled_for_loop_let_creates_per_iteration_envs() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(){
        let a;
        for (let i = 0; i < 3; i = i + 1) {
          if (i < 1) a = function(){ return i; };
        }
        return a();
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());

  let mut scope = heap.scope();
  let name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script: script.clone(),
      body: f_body,
    },
    name,
    0,
  )?;

  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Number(0.0));
  Ok(())
}

#[test]
fn compiled_bigint_literal_executes() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        return 0xFFn;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());

  let mut scope = heap.scope();
  let name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script: script.clone(),
      body: f_body,
    },
    name,
    0,
  )?;

  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  let expected = scope.alloc_bigint_from_u128(255)?;
  assert!(result.same_value(Value::BigInt(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_new_target_is_undefined_in_normal_call() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        return new.target;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let mut scope = heap.scope();
  let name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script: script.clone(),
      body: f_body,
    },
    name,
    0,
  )?;

  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_execution_respects_fuel_budget_in_infinite_loop() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        while (true) {}
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(100),
    deadline: None,
    check_time_every: 1,
  });

  let mut scope = heap.scope();
  let name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script: script.clone(),
      body: f_body,
    },
    name,
    0,
  )?;

  let err = vm
    .call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])
    .unwrap_err();
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected OutOfFuel termination, got {other:?}"),
  }

  Ok(())
}

#[test]
fn compiled_execution_is_gc_safe_under_stress() -> Result<(), VmError> {
  // Force a GC on every allocation.
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  let mut vm = Vm::new(VmOptions::default());

  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(n) {
        let i = 0;
        let o = null;
        while (i < n) {
          o = {};
          i = i + 1;
        }
        return o;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut scope = heap.scope();
  let name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script,
      body: f_body,
    },
    name,
    1,
  )?;

  let result = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Number(25.0)],
  )?;
  assert!(matches!(result, Value::Object(_)));
  assert!(
    scope.heap().gc_runs() > 0,
    "expected at least one GC cycle to run"
  );

  Ok(())
}

#[test]
fn compiled_object_literal_inherits_from_object_prototype() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(&mut rt.heap, "test.js", "({}).hasOwnProperty('x')")?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(false));
  Ok(())
}

#[test]
fn compiled_object_literal_object_spread_copies_properties() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let a = { x: 1 };
      let b = { ...a, y: 2 };
      b.x + b.y
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_member_get_boxes_primitive_base_via_to_object() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(&mut rt.heap, "test.js", "'abc'.length")?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_member_call_boxes_primitive_base_via_to_object() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(&mut rt.heap, "test.js", "'abc'.toUpperCase()")?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("ABC")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

fn proxy_get_trap(
  _vm: &mut Vm,
  _scope: &mut vm_js::Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Return a value that differs from any value stored on the target so tests can prove the trap
  // was invoked.
  Ok(Value::Number(2.0))
}

fn proxy_set_trap_return_false(
  _vm: &mut Vm,
  _scope: &mut vm_js::Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Bool(false))
}

#[test]
fn compiled_member_get_dispatches_proxy_get_trap() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(o) { return o.x; }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let call_id = vm.register_native_call(proxy_get_trap)?;

  let mut scope = heap.scope();

  // Target: { x: 1 }
  let target = scope.alloc_object()?;
  scope.push_root(Value::Object(target))?;
  let x_key_s = scope.alloc_string("x")?;
  scope.push_root(Value::String(x_key_s))?;
  let x_key = vm_js::PropertyKey::from_string(x_key_s);
  scope.define_property(
    target,
    x_key,
    vm_js::PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: vm_js::PropertyKind::Data {
        value: Value::Number(1.0),
        writable: true,
      },
    },
  )?;

  // Handler: { get: <native trap> }
  let handler = scope.alloc_object()?;
  scope.push_root(Value::Object(handler))?;
  let get_name = scope.alloc_string("get")?;
  scope.push_root(Value::String(get_name))?;
  let get_fn = scope.alloc_native_function(call_id, None, get_name, 3)?;
  scope.push_root(Value::Object(get_fn))?;
  let get_key_s = scope.alloc_string("get")?;
  scope.push_root(Value::String(get_key_s))?;
  let get_key = vm_js::PropertyKey::from_string(get_key_s);
  scope.define_property(
    handler,
    get_key,
    vm_js::PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: vm_js::PropertyKind::Data {
        value: Value::Object(get_fn),
        writable: true,
      },
    },
  )?;

  let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
  scope.push_root(Value::Object(proxy))?;

  let f_name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script,
      body: f_body,
    },
    f_name,
    1,
  )?;

  // f(proxy) should return the get-trap result (2), not the target's stored x (1).
  let result = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Object(proxy)],
  )?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_member_set_dispatches_proxy_set_trap() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(o) { o.x = 3; return o.x; }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let call_id = vm.register_native_call(proxy_set_trap_return_false)?;

  let mut scope = heap.scope();

  // Target: { x: 1 }
  let target = scope.alloc_object()?;
  scope.push_root(Value::Object(target))?;
  let x_key_s = scope.alloc_string("x")?;
  scope.push_root(Value::String(x_key_s))?;
  let x_key = vm_js::PropertyKey::from_string(x_key_s);
  scope.define_property(
    target,
    x_key,
    vm_js::PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: vm_js::PropertyKind::Data {
        value: Value::Number(1.0),
        writable: true,
      },
    },
  )?;

  // Handler: { set: <native trap> }
  let handler = scope.alloc_object()?;
  scope.push_root(Value::Object(handler))?;
  let set_name = scope.alloc_string("set")?;
  scope.push_root(Value::String(set_name))?;
  let set_fn = scope.alloc_native_function(call_id, None, set_name, 4)?;
  scope.push_root(Value::Object(set_fn))?;
  let set_key_s = scope.alloc_string("set")?;
  scope.push_root(Value::String(set_key_s))?;
  let set_key = vm_js::PropertyKey::from_string(set_key_s);
  scope.define_property(
    handler,
    set_key,
    vm_js::PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: vm_js::PropertyKind::Data {
        value: Value::Object(set_fn),
        writable: true,
      },
    },
  )?;

  let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
  scope.push_root(Value::Object(proxy))?;

  let f_name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script,
      body: f_body,
    },
    f_name,
    1,
  )?;

  // f(proxy) should still observe x=1, because the set trap returns false and the compiled member
  // assignment must route through Proxy `[[Set]]` instead of performing an ordinary set on the
  // target.
  let result = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Object(proxy)],
  )?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_strict_equality_compares_strings_by_value() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let a = 'x';
        let b = 'x';
        return a === b;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());

  let mut scope = heap.scope();
  let name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script: script.clone(),
      body: f_body,
    },
    name,
    0,
  )?;

  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_strict_equality_compares_bigints_by_value() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let a = 1n;
        let b = 1n;
        return a === b;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());

  let mut scope = heap.scope();
  let name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script: script.clone(),
      body: f_body,
    },
    name,
    0,
  )?;

  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_var_is_hoisted_in_function_body() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        return x;
        var x = 1;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());

  let mut scope = heap.scope();
  let name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script: script.clone(),
      body: f_body,
    },
    name,
    0,
  )?;

  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_var_declaration_without_initializer_is_noop() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        var x = 1;
        x = 2;
        var x;
        return x;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());

  let mut scope = heap.scope();
  let name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script: script.clone(),
      body: f_body,
    },
    name,
    0,
  )?;

  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_regex_literal_executes() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      /a/.test('a')
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_with_statement_executes() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let o = {x: 1};
      with (o) { x = 2; }
      o.x
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_try_catch_binds_exception_value() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"try { throw 1 } catch (e) { e + 1 }"#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_try_finally_runs_and_preserves_return() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let log='';
      function f(){ try { log = log + 'a'; return 1; } finally { log = log + 'b'; } }
      let r = f();
      log + r
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "ab1");
  Ok(())
}

#[test]
fn compiled_try_catch_coerces_internal_type_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      try { (null).x } catch(e) { ok = 1; }
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_lexical_tdz_shadowing_throws() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x = 1;
      { x; let x = 2; }
      'no'
    "#,
  )?;

  let err = rt.exec_compiled_script(script).unwrap_err();
  match err {
    VmError::ThrowWithStack { .. } => Ok(()),
    other => panic!("expected ThrowWithStack, got {other:?}"),
  }
}

#[test]
fn compiled_array_literal_basic() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let a = [1, 2, 3];
      a[0] + a[2];
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(4.0));
  Ok(())
}

#[test]
fn compiled_array_literal_holes_set_length() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let a = [1, , 3];
      a.length;
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_array_literal_spread() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let b = [2, 3];
      let a = [1, ...b, 4];
      a[1] + a[2];
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(5.0));
  Ok(())
}

#[test]
fn compiled_array_literal_uses_array_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let a = [1];
      a.push(2);
      a.length;
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_template_literal_executes() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let x = 2;
      `a${x}b`
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap_mut().scope();
  let expected = scope.alloc_string("a2b")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_switch_basic_match_break() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x = 'b';
      switch (x) {
        case 'a': 1; break;
        case 'b': 2; break;
        default: 3;
      }
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_switch_default_path() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x = 'c';
      switch (x) { case 'a': 1; break; default: 3; }
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_postfix_update_expression_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let i = 1;
      i++;
      i
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_member_update_expression_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let o = {x: 1};
      o.x++;
      o.x
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_numeric_add_assign_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let i = 1;
      i += 2;
      i
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_string_add_assign_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let s = 'a';
      s += 'b';
      s
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let mut scope = rt.heap_mut().scope();
  let expected = scope.alloc_string("ab")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_typeof_on_unbound_identifier_returns_undefined_string() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() { return typeof notDefined; }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());

  let mut scope = heap.scope();
  let name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script,
      body: f_body,
    },
    name,
    0,
  )?;

  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  let result = scope.push_root(result)?;
  let expected = scope.alloc_string("undefined")?;
  assert!(
    result.same_value(Value::String(expected), scope.heap()),
    "expected typeof result to be 'undefined', got {result:?}"
  );
  Ok(())
}

#[test]
fn compiled_in_operator_works() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() { return 'x' in ({x: 1}); }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());

  let mut scope = heap.scope();
  let name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script,
      body: f_body,
    },
    name,
    0,
  )?;

  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_bitwise_and_operator_works() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() { return 5 & 3; }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());

  let mut scope = heap.scope();
  let name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script,
      body: f_body,
    },
    name,
    0,
  )?;

  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_shift_left_operator_works() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() { return 1 << 3; }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());

  let mut scope = heap.scope();
  let name = scope.alloc_string("f")?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script,
      body: f_body,
    },
    name,
    0,
  )?;

  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Number(8.0));
  Ok(())
}

#[test]
fn compiled_instanceof_true_object_create_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function C() {}
      let o = Object.create(C.prototype);
      o instanceof C
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_instanceof_false_plain_object() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function C() {}
      ({}) instanceof C
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(false));
  Ok(())
}

#[test]
fn compiled_instanceof_rhs_not_object_throws() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", "1 instanceof 2")?;
  let err = rt.exec_compiled_script(script).unwrap_err();
  match err {
    VmError::ThrowWithStack { .. } | VmError::Throw(_) | VmError::TypeError(_) => Ok(()),
    other => panic!("expected TypeError, got {other:?}"),
  }
}
