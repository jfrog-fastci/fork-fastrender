use std::sync::Arc;
use vm_js::{
  Budget, CompiledFunctionRef, CompiledScript, Heap, HeapLimits, JsRuntime, TerminationReason, Value,
  Vm, VmError, VmHost, VmHostHooks, VmOptions,
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

fn assert_compiled_script_bigint(source: &str, expected: i128) -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", source)?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap_mut().scope();
  let expected = scope.alloc_bigint_from_i128(expected)?;
  assert!(result.same_value(Value::BigInt(expected), scope.heap()));
  Ok(())
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
fn compiled_for_of_let_creates_per_iteration_envs() -> Result<(), VmError> {
  // `for (let x of ...)` should create a fresh lexical binding each iteration so closures capture
  // the value from the iteration when they were created.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        let a;
        for (let i of [0, 1, 2]) {
          if (i < 1) a = function() { return i; };
        }
        return a();
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(0.0));
  Ok(())
}

#[test]
fn compiled_for_in_let_creates_per_iteration_envs() -> Result<(), VmError> {
  // `for (let k in obj)` should create a fresh lexical binding each iteration so closures capture
  // the key from the iteration when they were created.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        let a;
        for (let k in ({a: 1, b: 2})) {
          if (k === 'a') a = function() { return k; };
        }
        return a();
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "a");
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
fn compiled_bigint_unary_minus_executes() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        return -1n;
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
  let expected = scope.alloc_bigint_from_i128(-1)?;
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
fn compiled_new_target_is_constructor_function_in_new_call() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function C() {
        this.ok = new.target === C;
      }
      let o = new C();
      o.ok === true
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_arrow_function_captures_lexical_new_target_in_plain_call() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        return (() => new.target)();
      }
      f() === undefined
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_arrow_function_captures_lexical_new_target_in_constructor() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function C() {
        this.ok = (() => new.target === C)();
      }
      let o = new C();
      o.ok === true
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_constructor_can_return_arrow_capturing_new_target() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function C() {
        return () => new.target;
      }
      let f = new C();
      f() === C
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_reflect_construct_threads_new_target() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function Base() {
        this.ok = new.target === Derived;
      }
      function Derived() {}
      let o = Reflect.construct(Base, [], Derived);
      o.ok === true && Object.getPrototypeOf(o) === Derived.prototype
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_strict_equality_compares_string_contents() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        return ("ab" + "c") === "abc";
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
fn compiled_strict_equality_compares_bigint_values() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(a, b) {
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
    2,
  )?;

  // Allocate two equal BigInts that are guaranteed to have different GC handles.
  let a = scope.alloc_bigint_from_u128(3)?;
  scope.push_root(Value::BigInt(a))?;
  let b = scope.alloc_bigint_from_u128(3)?;
  scope.push_root(Value::BigInt(b))?;

  let result = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::BigInt(a), Value::BigInt(b)],
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_template_literal_concatenates() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let x = 2;
        return `a${x}b`;
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
  let expected = scope.alloc_string("a2b")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_unary_plus_coerces_object() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        return +({ valueOf() { return 3; } });
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let result = {
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

    vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?
  };
  assert_eq!(result, Value::Number(3.0));

  // Avoid leaking persistent roots (and tripping the Realm drop assertion).
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_unary_minus_bigint() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        return -1n;
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
  let expected = scope.alloc_bigint_from_i128(-1)?;
  assert!(result.same_value(Value::BigInt(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_regex_literal_creates_regexp() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f(){ return /a+/g.test("caa"); }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
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
fn compiled_stmt_list_update_empty_roots_last_value_across_gc() -> Result<(), VmError> {
  // Force a GC on every allocation.
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      ({});
      { let x = 1; }
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(obj) = result else {
    panic!("expected object, got {result:?}");
  };
  assert!(rt.heap().is_valid_object(obj));
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
fn compiled_object_literal_object_spread_respects_member_order() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // `z` exists in the spread source and is overwritten by the later `z: 3` property, so spread
  // must occur between `y: 2` and `z: 3`.
  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let a = { x: 1, z: 10 };
      let o = { y: 2, ...a, z: 3 };
      o.x + o.y + o.z
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(6.0));
  Ok(())
}

#[test]
fn compiled_object_literal_object_spread_overwrites_earlier_keys() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      ({ x: 1, ...{ x: 2 } }).x
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_object_literal_getter_executes() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let o = { get x() { return 1; } };
      o.x
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_object_literal_getter_setter_receiver_semantics() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let o = { _x: 1, get x() { return this._x; }, set x(v) { this._x = v; } };
      o.x = 2;
      o.x
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_object_literal_accessor_names_are_prefixed() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let o = { get x() { return 1; }, set x(v) {} };
      Object.getOwnPropertyDescriptor(o, 'x').get.name + '|' +
        Object.getOwnPropertyDescriptor(o, 'x').set.name
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("get x|set x")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_object_literal_infers_function_names() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let o = {
        ["func"]: function() {},
        ["arrow"]: () => {},
        ["method"]() {},
      };
      o.func.name + '|' + o.arrow.name + '|' + o.method.name
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("func|arrow|method")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
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

#[test]
fn compiled_member_call_uses_primitive_this_for_strict_functions() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // In strict mode, `this` is not coerced by the call machinery. Member calls on primitive bases
  // should therefore observe `this` as the primitive value (not the boxed wrapper object).
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use strict";
      String.prototype.f = function() { return this === 'abc'; };
      'abc'.f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_member_assignment_to_primitive_throws_in_strict_mode() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use strict";
      let ok = 0;
      try { 'abc'.x = 1; } catch(e) { ok = 1; }
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_computed_member_key_evaluates_before_nullish_base_error() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // In `null[expr]`, `expr` is evaluated (and ToPropertyKey is applied) before the `null` base is
  // coerced via `ToObject` and throws a TypeError.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      try { null[(ok = 1, 'x')] } catch(e) {}
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_computed_member_assignment_key_evaluates_before_nullish_base_error() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      try { null[(ok = 1, 'x')] = 2; } catch(e) {}
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_member_assignment_to_nullish_base_does_not_evaluate_rhs() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // In `null[expr] = rhs`, `expr` is evaluated before the nullish base error, but the RHS is not
  // evaluated because the member reference evaluation throws before reaching RHS evaluation.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      function rhs() { ok = 2; return 0; }
      try { null[(ok = 1, 'x')] = rhs(); } catch(e) {}
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_simple_assignment_evaluates_member_lhs_before_rhs() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let log = '';
      let o = {};
      function key() { log += 'k'; return 'x'; }
      function rhs() { log += 'r'; return 1; }
      o[key()] = rhs();
      log
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("kr")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_simple_assignment_evaluates_member_lhs_even_if_rhs_throws() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      let o = {};
      function key() { ok = 1; return 'x'; }
      function boom() { throw 1; }
      try { o[key()] = boom(); } catch (e) {}
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_simple_assignment_evaluates_member_base_before_rhs() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let log = '';
      function base() { log += 'b'; return {}; }
      function rhs() { log += 'r'; return 1; }
      base().x = rhs();
      log
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("br")?;
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

fn proxy_delete_trap_return_false(
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
fn compiled_member_delete_dispatches_proxy_delete_trap() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(o) { return delete o.x; }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());
  let call_id = vm.register_native_call(proxy_delete_trap_return_false)?;

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

  // Handler: { deleteProperty: <native trap> }
  let handler = scope.alloc_object()?;
  scope.push_root(Value::Object(handler))?;
  let del_name = scope.alloc_string("deleteProperty")?;
  scope.push_root(Value::String(del_name))?;
  let del_fn = scope.alloc_native_function(call_id, None, del_name, 2)?;
  scope.push_root(Value::Object(del_fn))?;
  let del_key_s = scope.alloc_string("deleteProperty")?;
  scope.push_root(Value::String(del_key_s))?;
  let del_key = vm_js::PropertyKey::from_string(del_key_s);
  scope.define_property(
    handler,
    del_key,
    vm_js::PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: vm_js::PropertyKind::Data {
        value: Value::Object(del_fn),
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

  // `delete proxy.x` should call the deleteProperty trap. The trap returns false, so in sloppy mode
  // the delete operator should produce false and the target's property should remain.
  let result = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Object(proxy)],
  )?;
  assert_eq!(result, Value::Bool(false));

  let still_present = scope
    .heap()
    .object_get_own_property_with_tick(target, &x_key, || vm.tick())?
    .is_some();
  assert!(still_present, "expected target.x to remain when delete trap returns false");
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
fn compiled_strict_equality_string_literal_is_true() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        return 'x' === 'x';
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
fn compiled_strict_equality_compares_concatenated_strings_by_value() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        return ('x' + 'y') === 'xy';
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
fn compiled_strict_equality_bigint_literal_is_true() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        return 1n === 1n;
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
fn compiled_numeric_literal_object_key_is_canonicalized() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let o = {0x10: 1, 1_0: 2};
        return o["16"] === 1 && o["10"] === 2;
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
fn compiled_class_declaration_basic() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        class C { constructor() { this.x = 2; } }
        return (new C()).x;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());

  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  {
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
  }

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_class_expression_basic() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let C = class {
          constructor() { this.x = 1; }
          m() { return this.x; }
        };
        return (new C()).m();
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());

  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  {
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
    assert_eq!(result, Value::Number(1.0));
  }

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_function_length_counts_params_before_first_default() -> Result<(), VmError> {
  let source = r#"
    function f(a, b = 1, c) {}
    f.length
  "#;

  // Interpreter result (baseline).
  let mut rt = JsRuntime::new(
    Vm::new(VmOptions::default()),
    Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024)),
  )?;
  let expected = rt.exec_script(source)?;
  assert_eq!(expected, Value::Number(1.0));

  // Compiled HIR execution should match.
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(&mut heap, "test.js", source)?;
  let mut rt = JsRuntime::new(Vm::new(VmOptions::default()), heap)?;
  let actual = rt.exec_compiled_script(script)?;
  assert_eq!(actual, expected);

  Ok(())
}

#[test]
fn compiled_function_length_stops_at_default_param() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "function f(a,b=1,c){}; f.length;",
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_function_length_stops_at_rest_param() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "function f(a,...rest){}; f.length;",
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_class_length_stops_at_default_param() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "class C { constructor(a,b=1,c){} }; C.length;",
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_class_length_stops_at_rest_param() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "class C { constructor(a,...rest){} }; C.length;",
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
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
fn compiled_strict_equality_treats_nan_as_unequal_to_itself() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        return NaN === NaN;
      }
      f();
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(false));
  Ok(())
}

#[test]
fn compiled_function_use_strict_affects_this() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ "use strict"; return this; }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_function_use_strict_makes_unbound_assignment_throw_reference_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ "use strict"; x = 1; }
      f();
    "#,
  )?;

  let err = rt.exec_compiled_script(script).unwrap_err();
  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));
  let Value::Object(thrown_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };

  let intr = rt
    .vm
    .intrinsics()
    .expect("intrinsics should be initialized for JsRuntime");
  let thrown_proto = rt.heap().object_prototype(thrown_obj)?;
  assert_eq!(thrown_proto, Some(intr.reference_error_prototype()));
  Ok(())
}

#[test]
fn compiled_computed_member_object_key_uses_to_property_key() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let k = { toString(){ return 'x'; } };
      let o = { x: 1 };
      o[k]
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_computed_object_literal_key_uses_to_property_key() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let k = { toString(){ return 'x'; } };
      let o = { [k]: 2 };
      o.x
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_numeric_ops_call_toprimitive_on_objects() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function mul_obj() {
        return ({ valueOf() { return 2; } }) * 3;
      }
      function plus_obj() {
        return +({ valueOf() { return 4; } });
      }
    "#,
  )?;
  let mul_body = find_function_body(&script, "mul_obj");
  let plus_body = find_function_body(&script, "plus_obj");

  let mut vm = Vm::new(VmOptions::default());
  // `ToPrimitive` (used by `ToNumber` on objects) requires initialized intrinsics for @@toPrimitive.
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;
  {
    let mut scope = heap.scope();

    let mul_name = scope.alloc_string("mul_obj")?;
    let mul_fn = scope.alloc_user_function(
      CompiledFunctionRef {
        script: script.clone(),
        body: mul_body,
      },
      mul_name,
      0,
    )?;
    let mul_result = vm.call_without_host(&mut scope, Value::Object(mul_fn), Value::Undefined, &[])?;
    assert_eq!(mul_result, Value::Number(6.0));

    let plus_name = scope.alloc_string("plus_obj")?;
    let plus_fn = scope.alloc_user_function(
      CompiledFunctionRef {
        script,
        body: plus_body,
      },
      plus_name,
      0,
    )?;
    let plus_result = vm.call_without_host(&mut scope, Value::Object(plus_fn), Value::Undefined, &[])?;
    assert_eq!(plus_result, Value::Number(4.0));
  }

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_numeric_ops_root_lhs_across_rhs_eval_under_gc_stress() -> Result<(), VmError> {
  // Force a GC on every allocation so binary operator evaluation must keep the LHS alive while
  // evaluating the RHS.
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        return ({ valueOf() { return 2; } }) * ({ valueOf() { return 3; } });
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  // `ToPrimitive` (used by `ToNumber` on objects) requires initialized intrinsics for @@toPrimitive.
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;
  {
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
    assert_eq!(result, Value::Number(6.0));
  }

  realm.teardown(&mut heap);
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
        var x = 2;
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
fn compiled_var_assignment_before_decl_uses_hoisted_binding() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(){ x = 1; var x; return x; }
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
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_var_in_blocks_is_function_scoped() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(){ var x = 1; { var x = 2; } return x; }
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
fn compiled_with_statement_resolves_identifiers() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(){
        let o = {x: 3};
        with (o) { return x; }
      }
      f()
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
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
fn compiled_with_restores_outer_lexical_env() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let x = 0;
      let o = {x: 1};
      with (o) { x = 2; }
      x
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // The `with` body assignment must go to `o.x` (because `o` has an `x` property), and the outer
  // lexical `x` binding must remain unchanged after the `with` completes.
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(0.0));
  Ok(())
}

#[test]
fn compiled_with_honors_symbol_unscopables() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let x = 1;
      let o = {x: 2};
      o[Symbol.unscopables] = {x: true};
      with (o) { x }
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // `Symbol.unscopables` should block `x` from being resolved through `o`'s `with` environment.
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_with_assignment_honors_symbol_unscopables() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let x = 0;
      function getX() { return x; }
      let o = {x: 1};
      o[Symbol.unscopables] = {x: true};
      with (o) { x = 2; }
      getX() + o.x
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // `Symbol.unscopables` should block `x` from being resolved through `o`'s `with` environment,
  // so assignment must update the outer lexical binding (x=2) and leave o.x unchanged (1).
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_with_restores_outer_env_on_labeled_break() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let x = 0;
      let o = {x: 1};
      label: {
        with (o) { break label; }
      }
      x
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // The `with` env must be restored even when control exits via a labeled break.
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(0.0));
  Ok(())
}

#[test]
fn compiled_with_boxes_primitive_binding_object() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      with ("abc") { length }
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_with_resolves_prototype_chain_properties() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let p = {x: 1};
      let o = Object.create(p);
      with (o) { x }
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_with_assignment_to_prototype_property_creates_own_property() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let p = {x: 1};
      let o = Object.create(p);
      with (o) { x = 2; }
      o.x + p.x
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_with_getter_receiver_is_binding_object() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let p = { get x() { return this.y; } };
      let o = Object.create(p);
      o.y = 2;
      with (o) { x }
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_with_setter_receiver_is_binding_object() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let p = { set x(v) { this.y = v; } };
      let o = Object.create(p);
      with (o) { x = 3; }
      o.y
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_with_falls_back_to_outer_binding_when_property_missing() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let x = 0;
      let o = {};
      with (o) { x = 2; }
      x
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // If the `with` binding object does not have an `x` property, the identifier should resolve to
  // the outer lexical `x` binding.
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_with_to_object_throws_for_null() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let ok = 0;
      try { with (null) { } } catch (e) { ok = 1; }
      ok
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_with_restores_outer_env_on_throw() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let x = 1;
      function getX() { return x; }
      let o = {x: 2};
      try {
        with (o) { throw 0; }
      } catch (e) {
        x = 3;
      }
      getX()
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // The `catch` block must run in the outer lexical environment, not the `with` environment.
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
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
fn compiled_try_catch_tdz_shadowing_throws() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      try { throw 1 } catch (e) { e; let e = 2; }
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
fn compiled_uncaught_type_error_is_coerced_to_throw_with_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", r#"(null).x"#)?;
  let err = rt.exec_compiled_script(script).unwrap_err();

  let VmError::ThrowWithStack { value, .. } = err else {
    panic!("expected ThrowWithStack, got {err:?}");
  };
  let Value::Object(obj) = value else {
    panic!("expected thrown object, got {value:?}");
  };

  // The host boundary should have coerced the internal VmError::TypeError into a real TypeError
  // object when intrinsics are available.
  let type_error_proto = rt.realm().intrinsics().type_error_prototype();

  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(obj))?;
  assert_eq!(scope.heap().object_prototype(obj)?, Some(type_error_proto));
  Ok(())
}

#[test]
fn compiled_finally_overrides_throw_completion() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // `finally` throw should override the `try` throw.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      try {
        try { throw 1; } finally { throw 2; }
      } catch (e) { e }
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_finally_overrides_return_completion() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() { try { return 1; } finally { return 2; } }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_catch_without_param_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      try { throw 1; } catch { 2 }
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_catch_restores_outer_lexical_env() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Catch creates its own lexical environment; outer bindings must be restored afterwards.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x = 1;
      try { throw 1; } catch (e) { let x = 2; }
      x
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
fn compiled_for_let_tdz_shadowing_throws() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x = 1;
      for (let x = x; false; ) {}
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
fn compiled_class_tdz_shadowing_throws() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let C = 1;
      { C; class C {} }
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
fn compiled_switch_instantiates_lexical_decls_for_tdz() -> Result<(), VmError> {
  // The `let x` inside the switch should create a TDZ binding for the whole case block, so the
  // case selector expression `x` resolves to the uninitialized binding and throws (instead of
  // reading the outer `x`).
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x = 1;
      switch (0) {
        case x: 1; break;
        default: 2; let x = 3;
      }
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
fn compiled_switch_function_basic_match_and_break() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(x) {
        let y = 0;
        switch (x) {
          case 1:
            y = 1;
            break;
          case 2:
            y = 2;
            break;
          default:
            y = 3;
        }
        return y;
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
    1,
  )?;

  let r2 = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Number(2.0)],
  )?;
  assert_eq!(r2, Value::Number(2.0));

  let r9 = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Number(9.0)],
  )?;
  assert_eq!(r9, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_switch_function_fallthrough() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(x) {
        let y = 0;
        switch (x) {
          case 1:
            y += 1;
          case 2:
            y += 2;
        }
        return y;
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
    1,
  )?;

  let r1 = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Number(1.0)],
  )?;
  assert_eq!(r1, Value::Number(3.0));

  let r2 = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Number(2.0)],
  )?;
  assert_eq!(r2, Value::Number(2.0));
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
fn compiled_bigint_update_expression_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let i = 1n;
      i++;
      i
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let mut scope = rt.heap_mut().scope();
  let expected = scope.alloc_bigint_from_u128(2)?;
  assert!(result.same_value(Value::BigInt(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_update_expr_postfix_increments_identifier_in_function() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let x = 1;
        x++;
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
      script,
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
fn compiled_update_expr_postfix_updates_member_and_returns_old_value_in_function() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let o = { x: 1 };
        return o.x++ + o.x;
      }
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

  // `o.x++` returns 1 and leaves `o.x == 2`, so the sum is 3.
  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_compound_assignment_add_assign_in_function() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let x = 1;
        x += 2;
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
      script,
      body: f_body,
    },
    name,
    0,
  )?;

  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Number(3.0));
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
fn compiled_bigint_add_assign_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let i = 1n;
      i += 2n;
      i
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let mut scope = rt.heap_mut().scope();
  let expected = scope.alloc_bigint_from_u128(3)?;
  assert!(result.same_value(Value::BigInt(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_bigint_subtract_operator_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint("5n - 2n", 3)
}

#[test]
fn compiled_bigint_multiply_operator_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint("3n * 4n", 12)
}

#[test]
fn compiled_bigint_divide_operator_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint("7n / 2n", 3)
}

#[test]
fn compiled_bigint_remainder_operator_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint("7n % 2n", 1)
}

#[test]
fn compiled_bigint_exponent_operator_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint("2n ** 3n", 8)
}

#[test]
fn compiled_bigint_sub_assign_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint(
    r#"
      let i = 5n;
      i -= 2n;
      i
    "#,
    3,
  )
}

#[test]
fn compiled_bigint_mul_assign_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint(
    r#"
      let i = 3n;
      i *= 4n;
      i
    "#,
    12,
  )
}

#[test]
fn compiled_bigint_div_assign_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint(
    r#"
      let i = 7n;
      i /= 2n;
      i
    "#,
    3,
  )
}

#[test]
fn compiled_bigint_rem_assign_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint(
    r#"
      let i = 7n;
      i %= 2n;
      i
    "#,
    1,
  )
}

#[test]
fn compiled_bigint_exponent_assign_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint(
    r#"
      let i = 2n;
      i **= 3n;
      i
    "#,
    8,
  )
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
fn compiled_delete_property_removes_property() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let o = Object.create(null);
      o.x = 1;
      delete o.x;
      o.x
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_delete_unresolvable_identifier_returns_true_in_sloppy_mode() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      delete notDefined
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_delete_optional_chain_short_circuits_to_true() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let o = null;
      delete o?.x
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_delete_non_configurable_property_throws_in_strict_mode() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use strict";
      let o = {};
      Object.defineProperty(o, "x", { value: 1, configurable: false });
      let ok = 0;
      try { delete o.x; } catch (e) { ok = 1; }
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
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

#[test]
fn compiled_for_of_over_string_iterable() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let out = '';
      for (let ch of 'ab') { out = out + ch; }
      out
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "ab");
  Ok(())
}

#[test]
fn compiled_object_destructuring_assignment_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x;
      ({x} = {x: 1});
      x
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_for_in_over_object() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let out = '';
      for (let k in ({a:1, b:2})) { out = out + k; }
      out
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "ab");
  Ok(())
}

#[test]
fn compiled_object_destructuring_decl_default_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let {x = 1} = {};
      x
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_for_of_labeled_continue_targets_loop() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let out = '';
      outer: for (let ch of 'ab') {
        out = out + ch;
        continue outer;
      }
      out
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "ab");
  Ok(())
}

#[test]
fn compiled_for_in_labeled_continue_targets_loop() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let out = '';
      outer: for (let k in ({a:1, b:2})) {
        out = out + k;
        continue outer;
      }
      out
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "ab");
  Ok(())
}

#[test]
fn compiled_function_length_ignores_rest_param() -> Result<(), VmError> {
  let source = r#"
    function g(a, ...rest) {}
    g.length
  "#;

  // Interpreter result (baseline).
  let mut rt = JsRuntime::new(
    Vm::new(VmOptions::default()),
    Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024)),
  )?;
  let expected = rt.exec_script(source)?;
  assert_eq!(expected, Value::Number(1.0));

  // Compiled HIR execution should match.
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(&mut heap, "test.js", source)?;
  let mut rt = JsRuntime::new(Vm::new(VmOptions::default()), heap)?;
  let actual = rt.exec_compiled_script(script)?;
  assert_eq!(actual, expected);
  Ok(())
}

#[test]
fn compiled_function_level_use_strict_sets_this_to_undefined() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ 'use strict'; return this; }
      f()
    "#,
  )?;

  let value = rt.exec_compiled_script(script)?;
  assert_eq!(value, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_direct_eval_updates_local_var() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ var x = 1; eval('x = 2'); return x; }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_direct_eval_is_not_triggered_when_shadowed() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ var eval = function(s){ return s; }; return eval('x'); }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "x");
  Ok(())
}

#[test]
fn compiled_function_creates_arguments_object() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(a){ return arguments.length + arguments[0]; }
      f(2)
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_function_default_initializer_can_read_arguments() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(a, b = arguments[0]){ return b; }
      f(1)
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_rest_parameters_collect_remaining_args() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(a, ...rest) { return rest.length; }
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
    1,
  )?;

  // f() => rest.length == 0
  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Number(0.0));

  // f(1, 2, 3) => rest == [2, 3] => rest.length == 2
  let result = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)],
  )?;
  assert_eq!(result, Value::Number(2.0));

  Ok(())
}

#[test]
fn compiled_rest_parameters_support_indexing() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(...rest) { return rest[1]; }
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

  // f(1, 2, 3) => rest[1] == 2
  let result = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)],
  )?;
  assert_eq!(result, Value::Number(2.0));

  Ok(())
}

#[test]
fn compiled_arrow_function_inherits_arguments_from_outer_function() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function outer(a, b) {
        return (() => arguments.length)();
      }
      outer(1, 2)
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_default_param_ref_to_later_param_throws_tdz_error() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(a = b, b = 1) { return a; }
      let out = 'no';
      try { f(); } catch (e) { out = e.message; }
      out
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(
    rt.heap().get_string(s)?.to_utf8_lossy(),
    "Cannot access 'b' before initialization"
  );
  Ok(())
}

#[test]
fn compiled_for_of_assigns_to_member_target() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let o = {x: ''};
      for (o.x of 'ab') {}
      o.x
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "b");
  Ok(())
}

#[test]
fn compiled_for_in_assigns_to_member_target() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let o = {x: ''};
      for (o.x in ({a:1, b:2})) {}
      o.x
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "b");
  Ok(())
}

#[test]
fn compiled_strict_directive_after_other_directives() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use asm";
      "use strict";
      function f(){ return this === undefined; }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_object_destructuring_rest_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let {x, ...rest} = {x: 1, y: 2};
      rest.y
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_parameter_object_destructuring_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f({x}) { return x; }
      f({x: 2})
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_typeof_unbound_identifier() -> Result<(), VmError> {
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
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_update_prefix_postfix() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let i = 0;
        let a = i++;
        let b = ++i;
        return a * 100 + b * 10 + i;
      }
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

  // a == 0, b == 2, i == 2 => 0*100 + 2*10 + 2 == 22
  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Number(22.0));
  Ok(())
}

#[test]
fn compiled_bigint_update() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let n = 1n;
        n++;
        return n;
      }
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
  let expected = scope.alloc_bigint_from_u128(2)?;
  assert!(result.same_value(Value::BigInt(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_delete_member() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let o = { a: 1 };
        return delete o.a && ("a" in o) === false;
      }
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
fn compiled_named_function_expr_recursion_works() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let fact = function fact(n) {
          if (n <= 1) return 1;
          return n * fact(n - 1);
        };
        // Named function expressions must have an inner immutable name binding for recursion that
        // is independent of the outer lexical binding.
        let g = fact;
        fact = 0;
        return g(5);
      }
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
  assert_eq!(result, Value::Number(120.0));
  Ok(())
}
