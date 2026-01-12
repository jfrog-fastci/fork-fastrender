use std::sync::Arc;
use vm_js::{
  Budget, CompiledFunctionRef, CompiledScript, Heap, HeapLimits, TerminationReason, Value, Vm,
  VmError, VmHost, VmHostHooks, VmOptions,
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
