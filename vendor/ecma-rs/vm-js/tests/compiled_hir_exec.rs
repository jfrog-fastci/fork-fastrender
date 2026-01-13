use std::sync::Arc;
use vm_js::{
  Budget, CompiledFunctionRef, CompiledScript, Heap, HeapLimits, JsRuntime, TerminationReason, Value,
  PropertyKey, Vm, VmError, VmHost, VmHostHooks, VmOptions,
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

fn compile_and_call0(source: &str, func_name: &str) -> Result<Value, VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(&mut heap, "test.js", source)?;
  let f_body = find_function_body(&script, func_name);
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;
  let result: Result<Value, VmError> = (|| {
    let mut scope = heap.scope();
    let name = scope.alloc_string(func_name)?;
    let f = scope.alloc_user_function(
      CompiledFunctionRef {
        script,
        body: f_body,
      },
      name,
      0,
    )?;
    vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])
  })();
  realm.teardown(&mut heap);
  result
}

fn run_f0(source: &str) -> Result<Value, VmError> {
  compile_and_call0(source, "f")
}

fn assert_thrown_is_reference_error(rt: &JsRuntime, err: VmError) -> Result<(), VmError> {
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
fn compiled_for_loop_let_closure_captures_each_iteration_value() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let f0;
        let f1;
        let f2;
        for (let i = 0; i < 3; i = i + 1) {
          if (i === 0) f0 = function() { return i; };
          if (i === 1) f1 = function() { return i; };
          if (i === 2) f2 = function() { return i; };
        }
        return f0() + f1() + f2();
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_for_loop_let_labelled_continue_captures_each_iteration_value() -> Result<(), VmError> {
  // Labelled `continue` should be consumed by the labelled loop, and the loop must still create
  // per-iteration environments so closures capture the correct value.
  let result = compile_and_call0(
    r#"
      function f() {
        let f0;
        let f1;
        let idx = 0;
        outer: for (let i = 0; i < 2; i = i + 1) {
          for (let j = 0; j < 2; j = j + 1) {
            if (idx === 0) f0 = function() { return i; };
            if (idx === 1) f1 = function() { return i; };
            idx = idx + 1;
            continue outer;
          }
        }
        return f0() + f1();
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_for_loop_let_body_mutation_is_visible_to_closure() -> Result<(), VmError> {
  // Closures capture the *binding*, not a copy of the value. Mutating the loop variable inside the
  // iteration after creating the closure should be visible to that closure.
  let result = compile_and_call0(
    r#"
      function f() {
        let g;
        for (let i = 0; i < 1; i = i + 1) {
          g = function() { return i; };
          i = 5;
        }
        return g();
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(5.0));
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
fn compiled_for_of_const_creates_per_iteration_envs() -> Result<(), VmError> {
  // Same as the `let` test, but for `const`.
  let result = compile_and_call0(
    r#"
      function f() {
        let f0;
        let f1;
        let f2;
        for (const i of [0, 1, 2]) {
          if (i === 0) f0 = function() { return i; };
          if (i === 1) f1 = function() { return i; };
          if (i === 2) f2 = function() { return i; };
        }
        return f0() + f1() + f2();
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(3.0));
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
fn compiled_for_in_const_creates_per_iteration_envs() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        let f_a;
        let f_b;
        for (const k in ({a: 1, b: 2})) {
          if (k === 'a') f_a = function() { return k; };
          if (k === 'b') f_b = function() { return k; };
        }
        return f_a() + f_b();
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  // Order doesn't matter here; `f_a`/`f_b` are assigned based on key value.
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "ab");
  Ok(())
}

#[test]
fn compiled_for_of_var_does_not_create_per_iteration_envs() -> Result<(), VmError> {
  // `for (var x of ...)` should *not* create a fresh binding per iteration; closures capture the
  // single function-scoped binding and therefore observe the last assigned value.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        var a;
        for (var i of [0, 1, 2]) {
          if (i < 1) a = function() { return i; };
        }
        return a();
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_for_in_var_does_not_create_per_iteration_envs() -> Result<(), VmError> {
  // `for (var k in obj)` should *not* create a fresh binding per iteration; closures capture the
  // single function-scoped binding and therefore observe the last assigned value.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        var a;
        for (var k in ({a: 1, b: 2})) {
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
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "b");
  Ok(())
}

#[test]
fn compiled_for_in_var_is_declared_when_rhs_is_null() -> Result<(), VmError> {
  // Even though `for (var k in null) {}` does not execute the body, the `var` binding is created.
  let result = compile_and_call0(
    r#"
      function f() {
        for (var k in null) {}
        return k;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_for_of_var_is_declared_even_when_rhs_throws() -> Result<(), VmError> {
  // `for..of` throws on null/undefined RHS, but `var` bindings are still created during function
  // instantiation.
  let result = compile_and_call0(
    r#"
      function f() {
        try {
          for (var x of null) {}
        } catch (e) {}
        return x;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_for_triple_var_does_not_create_per_iteration_binding() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        let a;
        for (var i = 0; i < 3; i = i + 1) {
          if (i < 1) a = function() { return i; };
        }
        return a();
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_for_triple_let_restores_lexical_env_on_break() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        for (let i = 0; i < 1; i = i + 1) {
          break;
        }
        return i;
      }
      f()
    "#,
  )?;

  let err = rt.exec_compiled_script(script).unwrap_err();
  assert!(matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }));
  Ok(())
}

#[test]
fn compiled_for_loop_let_restores_lexical_env_after_normal_completion() -> Result<(), VmError> {
  // The loop variable binding must not leak into the surrounding scope after the loop completes
  // normally.
  let result = compile_and_call0(
    r#"
      function f() {
        for (let i = 0; i < 1; i = i + 1) {}
        return typeof i === 'undefined';
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_for_loop_let_restores_lexical_env_after_throw() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"for (let i = 0; i < 1; i = i + 1) { throw 1; }"#,
  )?;
  let err = rt.exec_compiled_script(script).unwrap_err();
  assert!(matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }));

  // If the loop's lexical environment is not restored when the body throws, the loop variable
  // binding would leak into subsequent script executions.
  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", "typeof i")?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "undefined");
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
fn compiled_bigint_bitwise_or_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint("1n | 2n", 3)
}

#[test]
fn compiled_bigint_shift_left_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint("1n << 2n", 4)
}

#[test]
fn compiled_bigint_shift_left_negative_count_reverses_direction() -> Result<(), VmError> {
  // Match interpreter semantics: `x << -y` is `x >> y`.
  assert_compiled_script_bigint("8n << -1n", 4)
}

#[test]
fn compiled_bigint_shift_right_negative_count_reverses_direction() -> Result<(), VmError> {
  // Match interpreter semantics: `x >> -y` is `x << y`.
  assert_compiled_script_bigint("8n >> -1n", 16)
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
fn compiled_new_target_is_undefined_in_inner_non_construct_call() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // `new.target` is per-function, not dynamically scoped. A nested non-arrow function called
  // normally must observe `new.target === undefined` even if created inside a constructor call.
  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function C() {
        return function() { return new.target; };
      }
      new C()() === undefined
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_array_literal_holes_and_length() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let a = [1,,3];
        return a.length === 3 && a[1] === undefined;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let res: Result<(), VmError> = (|| {
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
  })();

  realm.teardown(&mut heap);
  res
}

#[test]
fn compiled_array_literal_spread_join() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        return [1, ...[2,3], 4].join(',');
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let res: Result<(), VmError> = (|| {
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
    let expected = scope.alloc_string("1,2,3,4")?;
    assert!(result.same_value(Value::String(expected), scope.heap()));
    Ok(())
  })();

  realm.teardown(&mut heap);
  res
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
fn compiled_new_target_inside_constructor() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function C() {
        return { ok: new.target === C };
      }
      new C().ok === true
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
fn compiled_template_literal_interpolates_expression() -> Result<(), VmError> {
  // Force frequent GC to ensure template string construction roots intermediate values correctly.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(x) {
        return `a${x}c`;
      }
      f(1)
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  let actual = rt.heap().get_string(s)?.to_utf8_lossy();
  assert_eq!(actual, "a1c");
  Ok(())
}

#[test]
fn compiled_template_literal_coerces_null_and_undefined() -> Result<(), VmError> {
  // Force frequent GC to ensure template string construction roots intermediate values correctly.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        return `${null}${undefined}`;
      }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  let actual = rt.heap().get_string(s)?.to_utf8_lossy();
  assert_eq!(actual, "nullundefined");
  Ok(())
}

#[test]
fn compiled_template_literal_preserves_surrogate_code_units() -> Result<(), VmError> {
  // `\uD800` is a lone surrogate code unit which cannot be represented in Rust `String`.
  // The compiled path must preserve it via UTF-16 code units.
  let result = compile_and_call0(
    r#"
      function f() {
        return (`\uD800`).charCodeAt(0);
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(0xD800 as f64));
  Ok(())
}

#[test]
fn compiled_tagged_template_provides_raw_and_cooked_strings() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        function tag(strings) {
          return strings.raw[0].length * 10 + strings[0].length;
        }
        return tag`\n`;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(21.0));
  Ok(())
}

#[test]
fn compiled_tagged_template_invalid_escape_sets_cooked_to_undefined() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        function tag(strings) {
          return strings[0] === undefined && strings.raw[0] === "\\1";
        }
        return tag`\1`;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_tagged_template_uses_base_as_this_for_member_call() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let obj = {
          x: 41,
          tag(strings) { return this.x + strings[0].length; }
        };
        return obj.tag`a`;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(42.0));
  Ok(())
}

#[test]
fn compiled_tagged_template_optional_chaining_short_circuits() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let x = 0;
        let obj = null;
        let r = obj?.tag`a${x++}b`;
        return r === undefined && x === 0;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_tagged_template_caches_template_object_per_site() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        function tag(strings) { return strings; }
        function get() { return tag`x`; }
        return get() === get();
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_hir_exec_unary_minus_bigint() -> Result<(), VmError> {
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

// Keep the original test names referenced by older docs/notes; the `compiled_hir_exec_*` variants
// ensure `cargo test -p vm-js --tests compiled_hir_exec` actually executes these assertions (Cargo
// treats the trailing arg as a libtest filter, not a test-binary selector).
#[test]
fn compiled_unary_minus_bigint() -> Result<(), VmError> {
  compiled_hir_exec_unary_minus_bigint()
}

#[test]
fn compiled_hir_exec_unary_plus_coerces_object() -> Result<(), VmError> {
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
fn compiled_unary_plus_coerces_object() -> Result<(), VmError> {
  compiled_hir_exec_unary_plus_coerces_object()
}

#[test]
fn compiled_hir_exec_unary_plus_bigint_throws_type_error() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        try {
          +1n;
          return false;
        } catch (e) {
          return e instanceof TypeError;
        }
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_hir_exec_unary_minus_object_preserves_bigint() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        return -({ valueOf() { return 1n; } });
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());
  // `ToNumeric` on objects requires intrinsics for @@toPrimitive lookup.
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
    let expected = scope.alloc_bigint_from_i128(-1)?;
    assert!(result.same_value(Value::BigInt(expected), scope.heap()));
  }

  // Avoid leaking persistent roots (and tripping the Realm drop assertion).
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_hir_exec_unary_plus_symbol_throws_type_error() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        try {
          +Symbol("s");
          return false;
        } catch (e) {
          return e instanceof TypeError;
        }
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_hir_exec_unary_minus_symbol_throws_type_error() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        try {
          -Symbol("s");
          return false;
        } catch (e) {
          return e instanceof TypeError;
        }
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_relational_comparison_string_uses_lexicographic_order() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        return 'a' < 'b';
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));

  let result = compile_and_call0(
    r#"
      function f() {
        return '2' < '10';
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(false));

  let result = compile_and_call0(
    r#"
      function f() {
        return '2' < 10;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));

  Ok(())
}

#[test]
fn compiled_relational_comparison_supports_bigint_and_object_coercion() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        return 1n < 2n;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));

  let result = compile_and_call0(
    r#"
      function f() {
        return 2n > 1n;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));

  let result = compile_and_call0(
    r#"
      function f() {
        return 1n < 2;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));

  let result = compile_and_call0(
    r#"
      function f() {
        return 2 < 1n;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(false));

  // BigInt/string comparisons must parse the string as a BigInt rather than rounding through
  // Number.
  let result = compile_and_call0(
    r#"
      function f() {
        return '9007199254740993' > 9007199254740992n;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));

  // Invalid BigInt parses yield the spec's `undefined` result, which is treated as `false` by `<`.
  let result = compile_and_call0(
    r#"
      function f() {
        return '0.' < 1n;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(false));

  // Object operands are coerced via ToPrimitive (hint Number).
  let result = compile_and_call0(
    r#"
      function f() {
        return ({ valueOf() { return 1; } }) < 2;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));

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
fn compiled_regex_literal_respects_flags_and_constructor() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f(){ return /a+b/i.test('AAB'); }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_regex_literal_parsing_handles_char_classes_and_escaped_slash() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f(){ return /[\/]x/.test('/x'); }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_regex_literal_accepts_v_flag() -> Result<(), VmError> {
  // Basic `/v` flag support: ensure the compiled HIR path forwards the `v` flag to `%RegExp%`.
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f(){ return /a/v.test("a"); }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_regex_literal_parses_escaped_slash_delimiter() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // The pattern contains an escaped `/` delimiter (`\/`). The compiled HIR path must not split the
  // literal early at that escaped slash.
  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f(){ return /a\/b/.test("a/b"); }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_regex_literal_parses_slash_in_character_class() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // The pattern contains an unescaped `/` inside a character class (`[/]`). The compiled HIR path
  // must ignore that slash when searching for the closing delimiter.
  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f(){ return /[/]/.test("/"); }
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
fn compiled_binary_operator_roots_lhs_across_gc() -> Result<(), VmError> {
  // Force a GC on every allocation. The RHS string literal allocation should not be allowed to
  // collect the LHS temporary object value before the `*` operation performs `ToNumber`.
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      ({ valueOf() { return 2; } }) * '3'
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(6.0));
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
fn compiled_object_literal_in_function_inherits_from_object_prototype() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        let o = {};
        // `toString` should be inherited from %Object.prototype%.
        return o.hasOwnProperty('toString');
      }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(false));
  Ok(())
}

#[test]
fn compiled_object_literal_proto_property_sets_prototype() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let proto = { y: 1 };
      let o = { __proto__: proto };
      Object.getPrototypeOf(o) === proto &&
        Object.getOwnPropertyDescriptor(o, "__proto__") === undefined
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_object_literal_proto_property_non_object_is_ignored() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let o = { __proto__: 1, x: 2 };
      Object.getPrototypeOf(o) === Object.prototype &&
        Object.getOwnPropertyDescriptor(o, "__proto__") === undefined &&
        o.x === 2
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_object_literal_computed_proto_key_creates_data_property() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let o = { ["__proto__"]: 1 };
      Object.getPrototypeOf(o) === Object.prototype &&
        Object.getOwnPropertyDescriptor(o, "__proto__").value === 1
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
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
fn compiled_object_literal_object_spread_boxes_string_primitives() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // Object spread should apply `ToObject` to non-nullish primitives (CopyDataProperties).
  //
  // Strings have enumerable index properties, so spreading a string should copy those indices.
  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let o = { ...'ab' };
      o[0] + o[1]
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
fn compiled_object_literal_object_spread_copies_symbol_properties() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let sym = Symbol("s");
      let a = {};
      a[sym] = 1;
      let o = { ...a };
      o[sym]
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_object_literal_object_spread_invokes_getter_once() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // CopyDataProperties should use `Get` on enumerable properties, so accessors are invoked and the
  // resulting value is copied as a data property.
  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let log = "";
      let src = { get x() { log += "x"; return 1; } };
      let o = { ...src };
      let before = log;
      let val = o.x;
      let after = log;
      before + ":" + after + ":" + val
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "x:x:1");
  Ok(())
}

#[test]
fn compiled_object_literal_object_spread_does_not_trigger_proto_setter() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // CopyDataProperties uses CreateDataProperty (not assignment), so spreading an own `"__proto__"`
  // data property must not set the target's prototype.
  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let proto = { y: 1 };
      let src = {};
      Object.defineProperty(src, "__proto__", { value: proto, enumerable: true, configurable: true, writable: true });
      let o = { ...src };
      let desc = Object.getOwnPropertyDescriptor(o, "__proto__");
      Object.getPrototypeOf(o) === Object.prototype &&
        desc !== undefined &&
        desc.value === proto
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_object_literal_object_spread_skips_non_enumerable_getters() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let log = "";
      let src = {};
      Object.defineProperty(src, "x", { get() { log += "x"; return 1; }, enumerable: false, configurable: true });
      let o = { ...src };
      log === "" && Object.getOwnPropertyDescriptor(o, "x") === undefined
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
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
fn compiled_object_literal_object_spread_in_function_respects_member_order() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        let a = { x: 1 };
        let o = { y: 2, ...a, z: 3 };
        return o.x + o.y + o.z;
      }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(6.0));
  Ok(())
}

#[test]
fn compiled_object_literal_object_spread_in_function_overwrites_earlier_keys() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        return ({ x: 1, ...{ x: 2 } }).x;
      }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_object_spread_copies_properties() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(){ let o = {a:1, ...{b:2}}; return o.a + o.b; }
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
fn compiled_object_spread_ignores_nullish() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(){ let o = {a:1, ...null, ...undefined}; return o.a; }
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
fn compiled_object_literal_getter_works() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(){
        let o = { get x(){ return 3; } };
        return o.x;
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
fn compiled_object_literal_setter_works() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(){
        let v = 0;
        let o = { set x(n){ v = n; } };
        o.x = 5;
        return v;
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
  assert_eq!(result, Value::Number(5.0));
  Ok(())
}

#[test]
fn compiled_object_literal_getter_returns_2() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(){
        let o = { get x(){ return 2; } };
        return o.x;
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
fn compiled_object_literal_setter_sets_3() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(){
        let v = 0;
        let o = { set x(a){ v = a; } };
        o.x = 3;
        return v;
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
fn compiled_member_update_expression_invokes_getter_and_setter() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let log = '';
      let o = { get x() { log += 'g'; return 1; }, set x(v) { log += 's'; log += v; } };
      o.x++;
      log
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let mut scope = rt.heap_mut().scope();
  let result = scope.push_root(result)?;
  let expected = scope.alloc_string("gs2")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
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
fn compiled_object_literal_accessor_lengths_are_correct() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let o = { get x() { return 1; }, set x(v) {} };
      Object.getOwnPropertyDescriptor(o, 'x').get.length * 10 +
        Object.getOwnPropertyDescriptor(o, 'x').set.length
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_object_literal_accessor_is_enumerable_and_configurable() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let o = { get x() { return 1; }, set x(v) {} };
      let d = Object.getOwnPropertyDescriptor(o, 'x');
      d.enumerable === true && d.configurable === true &&
        (typeof d.get === 'function') && (typeof d.set === 'function')
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_object_literal_accessor_functions_are_not_constructable() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let o = { get x() { return 1; }, set x(v) {} };
      let d = Object.getOwnPropertyDescriptor(o, 'x');

      let ok =
        Object.prototype.hasOwnProperty.call(d.get, 'prototype') === false &&
        Object.prototype.hasOwnProperty.call(d.set, 'prototype') === false;

      try { new d.get(); ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }
      try { new d.set(); ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
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
        ["cls"]: class {},
      };
      o.func.name + '|' + o.arrow.name + '|' + o.method.name + '|' + o.cls.name
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("func|arrow|method|cls")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_object_literal_method_name_inferred() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f(){
        let o = { m(){}, n: function(){} };
        return o.m.name + "," + o.n.name;
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("m,n")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_assignment_expression_sets_function_names() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      var f;
      f = function() {};
      var o = {};
      o.f = function() {};
      o.c = class {};
      o.a = () => {};
      f.name + "|" + o.f.name + "|" + o.c.name + "|" + o.a.name
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("f|f|c|a")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_logical_assignment_sets_function_names() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      var f;
      f ||= function() {};
      var g = 1;
      g &&= function() {};
      var h = null;
      h ??= function() {};

      var o = { p: undefined, q: 1, r: null };
      o.p ||= function() {};
      o.q &&= function() {};
      o.r ??= function() {};

      f.name + "|" + g.name + "|" + h.name + "|" + o.p.name + "|" + o.q.name + "|" + o.r.name
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("f|g|h|p|q|r")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_object_literal_method_is_not_constructable() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let o = { m() {} };
      let has_proto = o.m.hasOwnProperty('prototype');
      let threw = false;
      try { new o.m(); } catch (e) { threw = e instanceof TypeError; }
      has_proto + '|' + threw
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("false|true")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_object_literal_accessor_is_not_constructable() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      let o = { get x() { return 1; }, set x(v) {} };
      let d = Object.getOwnPropertyDescriptor(o, 'x');
      let has_proto = d.get.hasOwnProperty('prototype');
      let threw = false;
      try { new d.get(); } catch (e) { threw = e instanceof TypeError; }
      has_proto + '|' + threw
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("false|true")?;
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
fn compiled_member_get_uses_primitive_this_for_strict_getters() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // For member gets on primitive bases, accessors should be invoked with Receiver = base. In strict
  // mode, the getter observes the unboxed primitive `this` value.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      Object.defineProperty(String.prototype, "g", {
        get: function() { "use strict"; return this === "abc"; },
      });
      "abc".g
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_member_set_uses_primitive_this_for_strict_setters() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // Like `[[Get]]`, `[[Set]]` should invoke accessors with Receiver = base. In strict mode the
  // setter should observe the unboxed primitive `this` value.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let seen = false;
      Object.defineProperty(String.prototype, "s", {
        set: function(v) { "use strict"; seen = this === "abc"; },
      });
      "abc".s = 1;
      seen
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
fn compiled_member_update_expression_to_primitive_throws_in_strict_mode() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use strict";
      let ok = 0;
      try { 'abc'.x++; } catch(e) { ok = 1; }
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_destructuring_member_assignment_to_primitive_is_silent_in_sloppy_mode() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      try { ({ x: 'abc'.x } = { x: 1 }); ok = 1; } catch (e) { ok = 2; }
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_destructuring_member_assignment_to_primitive_throws_in_strict_mode() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use strict";
      let ok = 0;
      try { ({ x: 'abc'.x } = { x: 1 }); } catch (e) { ok = 1; }
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_array_destructuring_member_assignment_to_primitive_is_silent_in_sloppy_mode() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      try { ['abc'.x] = [1]; ok = 1; } catch (e) { ok = 2; }
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_array_destructuring_member_assignment_to_primitive_throws_in_strict_mode() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use strict";
      let ok = 0;
      try { ['abc'.x] = [1]; } catch (e) { ok = 1; }
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
fn compiled_computed_member_update_key_evaluates_before_nullish_base_error() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      try { null[(ok = 1, 'x')]++; } catch(e) {}
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_computed_member_call_evaluates_key_before_nullish_base_error_but_not_args() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // In `null[expr](arg)`, `expr` is evaluated (and ToPropertyKey is applied) before the `null` base
  // is coerced via `ToObject` and throws a TypeError. The call arguments are **not** evaluated,
  // since the member reference evaluation throws before reaching argument evaluation.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      function arg() { ok = 2; return 0; }
      try { null[(ok = 1, 'x')](arg()); } catch(e) {}
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_optional_chaining_computed_member_call_short_circuits_without_evaluating_key_or_args() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // In `o?.[expr](arg)`, when `o` is nullish, the optional chain short-circuits to `undefined` and
  // does not evaluate either `expr` or `arg`.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      function arg() { ok = 2; return 0; }
      let o = null;
      o?.[(ok = 1, 'x')](arg());
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(0.0));
  Ok(())
}

#[test]
fn compiled_optional_chain_propagates_to_following_member_access() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let o = null;
        return o?.x.y;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_optional_chain_propagates_to_following_computed_member_access_and_skips_key() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let side = 0;
        let o = null;
        try { o?.x[(side = 1, 'y')]; } catch (e) {}
        return side;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(0.0));
  Ok(())
}

#[test]
fn compiled_parenthesized_optional_chain_does_not_propagate_to_following_computed_member_access() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let side = 0;
        let o = null;
        try { (o?.x)[(side = 1, 'y')]; } catch (e) {}
        return side;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_optional_chain_propagates_through_member_call() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let o = null;
        let ok = true;
        try { o?.m.n(); } catch (e) { ok = false; }
        return ok;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_parenthesized_optional_chain_callee_does_not_short_circuit_call() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let side = 0;
        function arg() { side = side + 1; }
        let o = null;
        try { (o?.m.n)(arg()); } catch (e) {}
        return side;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_delete_optional_chain_continuation_short_circuits_to_true() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let o = null;
        return delete o?.x.y;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_delete_parenthesized_optional_chain_continuation_does_not_short_circuit() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let o = null;
        let threw = false;
        try { delete (o?.x).y; } catch (e) { threw = true; }
        return threw;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
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
fn compiled_member_compound_add_assign_to_nullish_base_does_not_evaluate_rhs() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // Compound assignment should evaluate the LHS reference (including computed keys) before the RHS.
  // If the base throws during reference evaluation, RHS evaluation must not occur.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      function rhs() { ok = 2; return 0; }
      try { null[(ok = 1, 'x')] += rhs(); } catch(e) {}
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_object_destructuring_computed_member_target_delays_to_property_key() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // For destructuring assignment targets like `o[key()]`, the key *expression* is evaluated before
  // reading the source property value, but `ToPropertyKey` is delayed until `PutValue` (after `GetV`
  // and default evaluation). This matches interpreter behaviour + test262.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let log = '';
      let o = {};
      function key() {
        log += 'k';
        return { toString() { log += 't'; return 'x'; } };
      }
      function def() { log += 'd'; return 1; }
      let src = { get a() { log += 'g'; return undefined; } };
      ({a: o[key()] = def()} = src);
      log
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("kgdt")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_array_destructuring_computed_member_target_delays_to_property_key() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // For iterator destructuring assignment targets like `o[key()]`, the key *expression* is
  // evaluated before advancing the iterator, but `ToPropertyKey` is delayed until `PutValue` (after
  // consuming the iterator value and evaluating any default).
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let log = '';
      let o = {};
      function key() {
        log += 'k';
        return { toString() { log += 't'; return 'x'; } };
      }
      let it = {
        [Symbol.iterator]() {
          return {
            next() { log += 'n'; return { value: 1, done: true }; }
          };
        }
      };
      [o[key()]] = it;
      log
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("knt")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
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

#[test]
fn compiled_assignment_evaluates_lhs_before_rhs() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){
        let log = "";
        function key(){ log += "k"; return "x"; }
        function rhs(){ log += "r"; return 1; }
        let o = {};
        o[key()] = rhs();
        return log;
      }
      f();
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
fn compiled_assignment_sets_anonymous_function_name_for_binding() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){
        let g;
        g = function(){};
        return g.name;
      }
      f();
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("g")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_assignment_sets_anonymous_function_name_for_property() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){
        let o = {};
        o.h = function(){};
        return o.h.name;
      }
      f();
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("h")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
  Ok(())
}

#[test]
fn compiled_destructuring_name_inference_matches_interpreter() -> Result<(), VmError> {
  let source = r#"
    var o = {};
    var h, a, c;

    ({ x: o.f } = { x: function(){} });
    ([o.g] = [function(){}]);

    ({ h } = { h: function(){} });
    ({ a } = { a: () => {} });
    ({ c } = { c: class {} });

    // Names for binding targets should be inferred; member targets in destructuring currently rely
    // on `PutValue` semantics (no extra inference). Compare compiled vs interpreter.
    o.f.name + "|" + o.g.name + "|" + h.name + "|" + a.name + "|" + c.name
  "#;

  // Interpreter result (baseline).
  let mut rt_interp = JsRuntime::new(
    Vm::new(VmOptions::default()),
    Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024)),
  )?;
  let expected = rt_interp.exec_script(source)?;
  let Value::String(expected_s) = expected else {
    panic!("expected string, got {expected:?}");
  };
  let expected_text = rt_interp.heap.get_string(expected_s)?.to_utf8_lossy().to_string();

  // Compiled HIR execution should match.
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(&mut heap, "test.js", source)?;
  let mut rt_compiled = JsRuntime::new(Vm::new(VmOptions::default()), heap)?;
  let actual = rt_compiled.exec_compiled_script(script)?;
  let Value::String(actual_s) = actual else {
    panic!("expected string, got {actual:?}");
  };
  let actual_text = rt_compiled.heap.get_string(actual_s)?.to_utf8_lossy().to_string();

  assert_eq!(actual_text, expected_text);
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

fn proxy_own_keys_trap_returns_x(
  _vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Return `["x"]`.
  let arr = scope.alloc_array(1)?;

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(arr))?;

  let idx_key_s = scope.alloc_string("0")?;
  scope.push_root(Value::String(idx_key_s))?;
  let idx_key = vm_js::PropertyKey::from_string(idx_key_s);

  let x_s = scope.alloc_string("x")?;
  scope.push_root(Value::String(x_s))?;

  scope.define_property(
    arr,
    idx_key,
    vm_js::PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: vm_js::PropertyKind::Data {
        value: Value::String(x_s),
        writable: true,
      },
    },
  )?;

  Ok(Value::Object(arr))
}

fn proxy_get_own_property_descriptor_trap_enumerable(
  _vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Return `{ enumerable: true, configurable: true }`.
  let desc = scope.alloc_object()?;

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(desc))?;

  let enumerable_s = scope.alloc_string("enumerable")?;
  scope.push_root(Value::String(enumerable_s))?;
  let enumerable_key = vm_js::PropertyKey::from_string(enumerable_s);
  scope.define_property(
    desc,
    enumerable_key,
    vm_js::PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: vm_js::PropertyKind::Data {
        value: Value::Bool(true),
        writable: true,
      },
    },
  )?;

  let configurable_s = scope.alloc_string("configurable")?;
  scope.push_root(Value::String(configurable_s))?;
  let configurable_key = vm_js::PropertyKey::from_string(configurable_s);
  scope.define_property(
    desc,
    configurable_key,
    vm_js::PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: vm_js::PropertyKind::Data {
        value: Value::Bool(true),
        writable: true,
      },
    },
  )?;

  Ok(Value::Object(desc))
}

fn proxy_get_trap_return_target(
  _vm: &mut Vm,
  _scope: &mut vm_js::Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Proxy `get` trap signature is (target, property, receiver). This helper ignores the requested
  // property and returns `target`.
  Ok(args.get(0).copied().unwrap_or(Value::Undefined))
}

fn native_has_instance_returns_true(
  _vm: &mut Vm,
  _scope: &mut vm_js::Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Bool(true))
}

#[test]
fn compiled_user_function_is_constructable() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function C(x) { this.x = x; }
    "#,
  )?;
  let c_body = find_function_body(&script, "C");

  {
    let mut scope = heap.scope();
    let name = scope.alloc_string("C")?;
    let c = scope.alloc_user_function(
      CompiledFunctionRef {
        script,
        body: c_body,
      },
      name,
      1,
    )?;

    // new C(7).x === 7
    let obj = vm.construct_without_host(
      &mut scope,
      Value::Object(c),
      &[Value::Number(7.0)],
      Value::Object(c),
    )?;
    let Value::Object(obj) = obj else {
      panic!("expected object, got {obj:?}");
    };

    let x_key = vm_js::PropertyKey::from_string(scope.alloc_string("x")?);
    let x = vm.get(&mut scope, obj, x_key)?;
    assert_eq!(x, Value::Number(7.0));

    // Ordinary compiled functions should have a `.prototype` object.
    let prototype_key = vm_js::PropertyKey::from_string(scope.alloc_string("prototype")?);
    let proto = vm.get(&mut scope, c, prototype_key)?;
    assert!(matches!(proto, Value::Object(_)));
  }

  realm.teardown(&mut heap);
  Ok(())
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
fn compiled_object_spread_dispatches_proxy_get_trap() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(o) {
        let r = { ...o };
        return r.x;
      }
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

  // f(proxy) should return the proxy get-trap result (2), proving object spread performs `Get`
  // through the Proxy's `[[Get]]` internal method.
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
fn compiled_object_spread_dispatches_proxy_own_keys_trap() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(o) {
        let r = { ...o };
        return r.x;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let get_call_id = vm.register_native_call(proxy_get_trap)?;
  let own_keys_call_id = vm.register_native_call(proxy_own_keys_trap_returns_x)?;
  let gopd_call_id = vm.register_native_call(proxy_get_own_property_descriptor_trap_enumerable)?;

  let mut scope = heap.scope();

  // Target: {}
  let target = scope.alloc_object()?;
  scope.push_root(Value::Object(target))?;

  // Handler: { get, ownKeys, getOwnPropertyDescriptor }
  let handler = scope.alloc_object()?;
  scope.push_root(Value::Object(handler))?;

  // get
  let get_name = scope.alloc_string("get")?;
  scope.push_root(Value::String(get_name))?;
  let get_fn = scope.alloc_native_function(get_call_id, None, get_name, 3)?;
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

  // ownKeys
  let own_keys_name = scope.alloc_string("ownKeys")?;
  scope.push_root(Value::String(own_keys_name))?;
  let own_keys_fn = scope.alloc_native_function(own_keys_call_id, None, own_keys_name, 1)?;
  scope.push_root(Value::Object(own_keys_fn))?;
  let own_keys_key_s = scope.alloc_string("ownKeys")?;
  scope.push_root(Value::String(own_keys_key_s))?;
  let own_keys_key = vm_js::PropertyKey::from_string(own_keys_key_s);
  scope.define_property(
    handler,
    own_keys_key,
    vm_js::PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: vm_js::PropertyKind::Data {
        value: Value::Object(own_keys_fn),
        writable: true,
      },
    },
  )?;

  // getOwnPropertyDescriptor
  let gopd_name = scope.alloc_string("getOwnPropertyDescriptor")?;
  scope.push_root(Value::String(gopd_name))?;
  let gopd_fn = scope.alloc_native_function(gopd_call_id, None, gopd_name, 2)?;
  scope.push_root(Value::Object(gopd_fn))?;
  let gopd_key_s = scope.alloc_string("getOwnPropertyDescriptor")?;
  scope.push_root(Value::String(gopd_key_s))?;
  let gopd_key = vm_js::PropertyKey::from_string(gopd_key_s);
  scope.define_property(
    handler,
    gopd_key,
    vm_js::PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: vm_js::PropertyKind::Data {
        value: Value::Object(gopd_fn),
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

  // If the spread correctly uses Proxy `ownKeys` + `getOwnPropertyDescriptor`, it will enumerate the
  // synthetic key `"x"` and then read it via the Proxy get trap, returning 2.
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
fn compiled_class_methods_and_accessors_are_not_constructors() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      class C {
        m() {}
        static sm() {}
        get x() { return 1; }
        set x(v) {}
      }

      let ok = true;
      ok = ok && (new C()).m.name === "m";
      ok = ok && C.sm.name === "sm";
      ok = ok && (new C()).m.prototype === undefined;
      ok = ok && C.sm.prototype === undefined;

      const d = Object.getOwnPropertyDescriptor(C.prototype, "x");
      ok = ok && d.get.name === "get x";
      ok = ok && d.set.name === "set x";
      ok = ok && d.get.prototype === undefined;
      ok = ok && d.set.prototype === undefined;

      try { new (new C()).m(); ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }
      try { new C.sm(); ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }
      try { new (d.get)(); ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }
      try { new (d.set)(); ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }

      ok
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_numeric_literal_keys_are_canonicalized() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      class C {
        0x10() { return 1; }
        1_0() { return 2; }
        static 0b11() { return 3; }
      }

      let c = new C();
      c["16"]() === 1 && c["10"]() === 2 && C["3"]() === 3
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_static_blocks_execute_after_methods_and_do_not_leak_var_decls() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      class C {
        static { this.y = this.m(); var leaked = 1; }
        static m() { return 3; }
      }

      typeof leaked === "undefined" && C.y === 3
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_constructor_and_method() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        class C {
          constructor(x) { this.x = x; }
          m() { return this.x; }
        }
        return (new C(3)).m();
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_class_static_method() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        class C { static s() { return 2; } }
        return C.s();
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_class_call_without_new_throws() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        class C {}
        try {
          C();
          return 0;
        } catch (e) {
          return 1;
        }
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_named_class_expression_has_inner_name_binding() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        let C = class D {
          static g() { return D; }
        };
        return C.g() === C && typeof D === "undefined";
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_decl_inner_binding_is_immutable_and_shields_methods_from_outer_reassign() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        class C {
          static self() { return C; }
          static tryReassignInner() {
            try { C = 1; return 0; } catch (e) { return 1; }
          }
        }

        let orig = C;
        C = 1;
        return orig.self() === orig && orig.tryReassignInner() === 1;
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_constructor_prototype_is_non_writable() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        class C {}
        const d = Object.getOwnPropertyDescriptor(C, "prototype");
        return d.writable === false;
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_default_class_constructor_uses_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        class C {}
        C.prototype.y = 1;
        return (new C()).y;
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_class_inheritance_sets_constructor_and_instance_prototype_chains() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      class B {}
      class D extends B {}
      Object.getPrototypeOf(D) === B &&
        Object.getPrototypeOf(D.prototype) === B.prototype
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_extends_null_sets_constructor_and_instance_prototypes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      class D extends null {}
      Object.getPrototypeOf(D) === Function.prototype &&
        Object.getPrototypeOf(D.prototype) === null
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_base_constructor_inherits_from_function_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      class C {}
      Object.getPrototypeOf(C) === Function.prototype
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_extends_undefined_throws_type_error_message() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = false;
      try {
        class D extends undefined {}
      } catch (e) {
        ok = e instanceof TypeError && e.message === "Class extends value is not a constructor";
      }
      ok
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_extends_non_constructor_throws_type_error_message() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = false;
      try {
        class D extends ({}) {}
      } catch (e) {
        ok = e instanceof TypeError && e.message === "Class extends value is not a constructor";
      }
      ok
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_extends_invalid_prototype_throws_type_error_message() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = false;
      function B() {}
      B.prototype = 1;
      try {
        class D extends B {}
      } catch (e) {
        ok = e instanceof TypeError &&
          e.message === "Class extends value does not have a valid prototype property";
      }
      ok
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_base_prototype_inherits_from_object_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      class C {}
      Object.getPrototypeOf(C.prototype) === Object.prototype
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_derived_default_constructor_calls_super_with_args() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      class B {
        constructor(x) { this.x = x; }
      }
      class D extends B {}
      (new D(3)).x
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_class_extends_null_default_constructor_throws_type_error_message() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      class D extends null {}
      let ok = false;
      try {
        new D();
      } catch (e) {
        ok = e instanceof TypeError && e.message === "Class extends value is not a constructor";
      }
      ok
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_derived_super_prototype_uses_proxy_get_trap() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let called = 0;
      class B {}
      let P = new Proxy(B, {
        get(target, key, receiver) {
          if (key === "prototype") called++;
          return Reflect.get(target, key, receiver);
        }
      });
      class D extends P {}
      called === 1 && Object.getPrototypeOf(D.prototype) === B.prototype
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_inheritance_is_gc_safe_under_stress() -> Result<(), VmError> {
  // Force a GC on every allocation.
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // The superclass is produced by a class expression in the `extends` clause; it must be rooted
  // across allocations during derived-class construction.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      class D extends (class B { constructor() { this.x = 1; } }) {}
      (new D()).x
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  assert!(
    rt.heap().gc_runs() > 0,
    "expected at least one GC cycle to run"
  );
  Ok(())
}

#[test]
fn compiled_class_constructor_can_return_object() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        class C {
          constructor() { return { x: 1 }; }
        }
        return (new C()).x;
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_class_constructor_return_primitive_is_ignored() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        class C {
          constructor() { this.x = 1; return 5; }
        }
        return (new C()).x;
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_computed_class_method_key_uses_to_property_key() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        let called = 0;
        let key = { toString() { called++; return "m"; } };
        class C {
          [key]() { return called; }
        }
        return (new C()).m();
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_class_methods_are_strict_mode() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        class C {
          static g() {
            try { undeclared = 1; return 0; } catch (e) { return 1; }
          }
        }
        return C.g();
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_assignment_sets_anonymous_class_name_for_binding() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        let C;
        C = class {};
        return C.name === "C";
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_assignment_sets_anonymous_class_name_for_property() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        let o = {};
        o.h = class {};
        return o.h.name === "h";
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_prototype_constructor_points_to_class() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        class C {}
        return C.prototype.constructor === C;
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_constructor_prototype_descriptor_flags() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        class C {}
        const d = Object.getOwnPropertyDescriptor(C, "prototype");
        return d.writable === false && d.enumerable === false && d.configurable === false;
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_methods_have_expected_property_attributes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        class C {
          m() { return 1; }
          static s() { return 2; }
        }

        const md = Object.getOwnPropertyDescriptor(C.prototype, "m");
        const sd = Object.getOwnPropertyDescriptor(C, "s");

        return (
          md.enumerable === false &&
          md.configurable === true &&
          md.writable === true &&
          typeof md.value === "function" &&
          sd.enumerable === false &&
          sd.configurable === true &&
          sd.writable === true &&
          typeof sd.value === "function"
        );
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_class_method_unbound_this_is_undefined() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        class C {
          m() { return this; }
        }
        const m = (new C()).m;
        return m() === undefined;
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_computed_class_method_symbol_key_works() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        const sym = Symbol("m");
        class C {
          [sym]() { return 1; }
        }
        return (new C())[sym]();
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
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
fn compiled_function_length_simple_params() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "function f(a,b,c){}; f.length;",
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
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
fn compiled_function_expression_length_stops_at_default_param() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "(function(a, b = 1, c) {}).length;",
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_function_expression_length_stops_at_rest_param() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "(function(a, ...rest) {}).length;",
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_arrow_function_length_stops_at_default_param() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "((a, b = 1, c) => {}).length;",
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_arrow_function_length_stops_at_rest_param() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "((a, ...rest) => {}).length;",
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
fn compiled_function_length_scan_consumes_fuel_budget() -> Result<(), VmError> {
  // The compiled-HIR function allocation path scans parameter metadata to compute `function.length`.
  // That scan must call `vm.tick()` periodically so pathological parameter lists cannot consume
  // unbounded fuel within a single statement.
  const PARAM_COUNT: usize = 8192;

  let mut source = String::new();
  // Approximate length: "function f(" + each param name (e.g. "a1234") + commas + "){}; f.length;"
  source.reserve(PARAM_COUNT * 8);
  source.push_str("function f(");
  for i in 0..PARAM_COUNT {
    if i > 0 {
      source.push(',');
    }
    source.push('a');
    source.push_str(&i.to_string());
  }
  source.push_str("){}; f.length;");

  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(200),
    deadline: None,
    check_time_every: 1,
  });
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", source)?;
  let err = rt.exec_compiled_script(script).unwrap_err();
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected OutOfFuel termination, got {other:?}"),
  }
  Ok(())
}

#[test]
fn compiled_param_binding_scan_consumes_fuel_budget() -> Result<(), VmError> {
  // Function calls pre-scan parameter patterns to create bindings (for TDZ/default semantics). That
  // traversal must call `vm.tick()` periodically so huge destructuring patterns can't do
  // uninterruptible work.
  const ELEM_COUNT: usize = 8192;

  let mut source = String::new();
  source.reserve(ELEM_COUNT * 8);
  source.push_str("function f([");
  for i in 0..ELEM_COUNT {
    if i > 0 {
      source.push(',');
    }
    source.push('a');
    source.push_str(&i.to_string());
  }
  source.push_str("]){}; f();");

  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(25),
    deadline: None,
    check_time_every: 1,
  });
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", source)?;
  let err = rt.exec_compiled_script(script).unwrap_err();
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected OutOfFuel termination, got {other:?}"),
  }
  Ok(())
}

#[test]
fn compiled_arguments_object_exists_and_has_length() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() { return arguments.length; }
      f(1, 2, 3);
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_arguments_visible_in_default_initializer() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(a = arguments.length) { return a; }
      f(undefined, 1, 2);
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_param_tdz_shadows_outer_binding() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var b = 100;
      function f(a = b, b = 2) { return a; }
      f(undefined, 2);
    "#,
  )?;

  let err = rt.exec_compiled_script(script).unwrap_err();
  let thrown_value = match err {
    VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected throw, got {other:?}"),
  };
  let Value::Object(err_obj) = thrown_value else {
    panic!("expected thrown object, got {thrown_value:?}");
  };

  // `ReferenceError.prototype.name === "ReferenceError"`.
  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(err_obj))?;
  let name_key = vm_js::PropertyKey::from_string(scope.alloc_string("name")?);
  let Some(desc) = scope.heap().get_property(err_obj, &name_key)? else {
    panic!("expected error.name property");
  };
  let vm_js::PropertyKind::Data { value, .. } = desc.kind else {
    panic!("expected error.name to be a data property");
  };
  let Value::String(name_val) = value else {
    panic!("expected error.name to be a string, got {value:?}");
  };
  let actual = scope.heap().get_string(name_val)?.to_utf8_lossy();
  assert_eq!(actual, "ReferenceError");
  Ok(())
}

#[test]
fn compiled_rest_parameter_collects_args() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(...xs) { return xs.length; }
      f(1, 2, 3);
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
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
      f
    "#,
  )?;
  let f = rt.exec_compiled_script(script)?;

  // Call through the compiled-function execution path (not just compiled-script execution) to
  // ensure `===` semantics match the interpreter even when the global `NaN` binding is involved.
  // Borrow-split `vm` and `heap` so we can hold a `Scope` while calling into the VM.
  let vm = &mut rt.vm;
  let heap = &mut rt.heap;
  let mut scope = heap.scope();
  scope.push_root(f)?;
  let result = vm.call_without_host(&mut scope, f, Value::Undefined, &[])?;
  assert_eq!(result, Value::Bool(false));
  Ok(())
}

#[test]
fn compiled_bigint_arithmetic_and_strict_equality() -> Result<(), VmError> {
  let result = run_f0(
    r#"
      function f() { return (1n + 2n) === 3n; }
    "#,
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_relational_string_compare_uses_abstract_relational_comparison() -> Result<(), VmError> {
  let result = run_f0(
    r#"
      function f() { return "a" < "b"; }
    "#,
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_relational_string_bigint_parse_semantics() -> Result<(), VmError> {
  let result = run_f0(
    r#"
      function f() { return "9007199254740993" > 9007199254740992n; }
    "#,
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_bitwise_or_operator_works() -> Result<(), VmError> {
  let result = run_f0(
    r#"
      function f() { return (5 | 2); }
    "#,
  )?;
  assert_eq!(result, Value::Number(7.0));
  Ok(())
}

#[test]
fn compiled_nullish_assignment() -> Result<(), VmError> {
  let result = run_f0(
    r#"
      function f() { let x = null; x ??= 5; return x; }
    "#,
  )?;
  assert_eq!(result, Value::Number(5.0));
  Ok(())
}

#[test]
fn compiled_bigint_mixing_throws_type_error() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() { return 1n - 1; }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let err = {
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
    vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])
      .unwrap_err()
  };

  match err {
    VmError::TypeError(msg) => assert_eq!(msg, "Cannot mix BigInt and other types"),
    VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
      let Value::Object(obj) = value else {
        panic!("expected thrown object, got {value:?}");
      };

      // Root the thrown value across key allocation.
      let mut check_scope = heap.scope();
      check_scope.push_root(Value::Object(obj))?;
      let key_s = check_scope.alloc_string("message")?;
      check_scope.push_root(Value::String(key_s))?;
      let key = vm_js::PropertyKey::from_string(key_s);
      let Some(desc) = check_scope.heap().object_get_own_property(obj, &key)? else {
        panic!("expected message property on thrown value");
      };
      let vm_js::PropertyKind::Data { value: msg, .. } = desc.kind else {
        panic!("expected data property for message");
      };
      let Value::String(msg_s) = msg else {
        panic!("expected message to be a string, got {msg:?}");
      };
      let msg = check_scope.heap().get_string(msg_s)?.to_utf8_lossy();
      assert_eq!(msg, "Cannot mix BigInt and other types");
    }
    other => panic!("expected TypeError, got {other:?}"),
  }

  realm.teardown(&mut heap);
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
fn compiled_function_use_strict_not_first_stmt_is_not_a_directive() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Directive prologues are only recognized at the start of a function body statement list. If any
  // non-directive statement appears first (like `0;`), later `"use strict"` string literals must not
  // enable strict mode.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ 0; "use strict"; return this === undefined; }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(false));
  Ok(())
}

#[test]
fn compiled_arrow_function_use_strict_makes_unbound_assignment_throw_reference_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      (() => { "use strict"; x = 1; })();
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
fn compiled_arrow_function_expression_body_use_strict_is_not_a_directive() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Only *block-bodied* arrow functions can have directive prologues. An expression-bodied arrow
  // like `() => ("use strict", x = 1)` must remain non-strict.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      (() => ("use strict", x = 1))();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_arrow_function_use_strict_is_strict_but_preserves_lexical_this() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Arrow functions are strict-mode capable (via directive prologues), but must always preserve
  // lexical `this`.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let o = {
        x: 1,
        f: function() {
          return (() => {
            "use strict";
            let this_ok = false;
            try { this_ok = this.x === 1; } catch (e) {}
            let threw = false;
            try { y = 1; } catch (e) { threw = true; }
            return this_ok && threw && (typeof y === "undefined");
          })();
        },
      };
      o.f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_arrow_function_inherits_strictness_from_strict_parent() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        "use strict";
        return (() => { x = 1; })();
      }
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
fn compiled_strict_block_function_decls_are_block_scoped() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){
        "use strict";
        {
          g();
          function g(){ return 1; }
        }
        try { g; return false; } catch(e) { return true; }
      }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_sloppy_block_function_decls_do_not_initialize_when_not_executed() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        if (false) {
          function g() { return 1; }
        }
        return typeof g;
      }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "undefined");
  Ok(())
}

#[test]
fn compiled_sloppy_block_function_decls_update_var_binding_when_executed() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        let before = typeof g;
        if (true) { function g() { return 1; } }
        let after = typeof g;
        return before + "," + after;
      }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "undefined,function");
  Ok(())
}

#[test]
fn compiled_sloppy_switch_case_function_decl_not_executed_leaves_undefined() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(x) {
        switch (x) {
          case 0:
            function g() { return 1; }
            break;
        }
        if (x === 0) return g();
        return typeof g;
      }
      f(1) + "," + f(0)
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "undefined,1");
  Ok(())
}

#[test]
fn compiled_nested_function_use_strict_directive_is_detected() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        return function() { "use strict"; return this; }();
      }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_nested_function_inherits_strictness_from_strict_parent() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        "use strict";
        return function() { return this; }();
      }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
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
fn compiled_computed_member_assignment_object_key_uses_to_property_key() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let k = { toString(){ return 'x'; } };
      let o = {};
      o[k] = 4;
      o.x
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(4.0));
  Ok(())
}

#[test]
fn compiled_computed_member_call_object_key_uses_to_property_key() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let k = { toString(){ return 'm'; } };
      let o = { x: 7, m(){ return this.x; } };
      o[k]()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(7.0));
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
fn compiled_computed_class_member_key_uses_to_property_key() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let k = { toString(){ return 'm'; } };
      class C { [k](){ return 3; } }
      (new C()).m()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
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
      function or_obj() {
        return ({ valueOf() { return 1; } }) | 2;
      }
      function shl_obj() {
        return ({ valueOf() { return 1; } }) << 2;
      }
      function ushr_obj() {
        return ({ valueOf() { return 8; } }) >>> 1;
      }
    "#,
  )?;
  let mul_body = find_function_body(&script, "mul_obj");
  let plus_body = find_function_body(&script, "plus_obj");
  let or_body = find_function_body(&script, "or_obj");
  let shl_body = find_function_body(&script, "shl_obj");
  let ushr_body = find_function_body(&script, "ushr_obj");

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
        script: script.clone(),
        body: plus_body,
      },
      plus_name,
      0,
    )?;
    let plus_result = vm.call_without_host(&mut scope, Value::Object(plus_fn), Value::Undefined, &[])?;
    assert_eq!(plus_result, Value::Number(4.0));

    let or_name = scope.alloc_string("or_obj")?;
    let or_fn = scope.alloc_user_function(
      CompiledFunctionRef {
        script: script.clone(),
        body: or_body,
      },
      or_name,
      0,
    )?;
    let or_result = vm.call_without_host(&mut scope, Value::Object(or_fn), Value::Undefined, &[])?;
    assert_eq!(or_result, Value::Number(3.0));

    let shl_name = scope.alloc_string("shl_obj")?;
    let shl_fn = scope.alloc_user_function(
      CompiledFunctionRef {
        script: script.clone(),
        body: shl_body,
      },
      shl_name,
      0,
    )?;
    let shl_result = vm.call_without_host(&mut scope, Value::Object(shl_fn), Value::Undefined, &[])?;
    assert_eq!(shl_result, Value::Number(4.0));

    let ushr_name = scope.alloc_string("ushr_obj")?;
    let ushr_fn = scope.alloc_user_function(
      CompiledFunctionRef {
        script,
        body: ushr_body,
      },
      ushr_name,
      0,
    )?;
    let ushr_result = vm.call_without_host(&mut scope, Value::Object(ushr_fn), Value::Undefined, &[])?;
    assert_eq!(ushr_result, Value::Number(4.0));
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
fn compiled_var_is_hoisted_in_script() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        return x;
      }
      f();
      var x = 1;
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_global_var_declaration_without_initializer_is_noop() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var x = 1;
      var x;
      x
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
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
fn compiled_var_is_hoisted_in_script_body() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      x === undefined;
      var x = 1;
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_var_in_function_body_is_not_hoisted_to_script_body() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() { var __vmjs_var_not_global__ = 1; }
      __vmjs_var_not_global__;
    "#,
  )?;
  let err = rt.exec_compiled_script(script).unwrap_err();
  assert!(matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }));
  Ok(())
}

#[test]
fn compiled_var_in_nested_function_is_not_hoisted_to_outer_function() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        function g() { var __vmjs_var_not_outer__ = 1; }
        return __vmjs_var_not_outer__;
      }
      f();
    "#,
  )?;
  let err = rt.exec_compiled_script(script).unwrap_err();
  assert!(matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }));
  Ok(())
}

#[test]
fn compiled_var_in_for_init_is_hoisted() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        __vmjs_i__; // read before for-loop var decl: should see `undefined`, not ReferenceError.
        for (var __vmjs_i__ = 0; __vmjs_i__ < 1; __vmjs_i__ = __vmjs_i__ + 1) {}
        return __vmjs_i__;
      }
      f();
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_var_in_for_in_head_is_hoisted_when_loop_body_not_entered() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        for (var __vmjs_k__ in ({})) {}
        return __vmjs_k__;
      }
      f();
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_var_in_for_of_head_is_hoisted_when_no_iterations() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        for (var __vmjs_k__ of []) {}
        return __vmjs_k__;
      }
      f();
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_var_in_switch_case_is_hoisted() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        switch (0) {
          case 1:
            var __vmjs_x__ = 1;
        }
        return __vmjs_x__;
      }
      f();
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_var_in_try_block_is_hoisted() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        try {
          throw 1;
          var __vmjs_x__ = 2;
        } catch (e) {}
        return __vmjs_x__;
      }
      f();
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_var_in_catch_block_is_hoisted() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        try {
          0;
        } catch (e) {
          var __vmjs_x__ = 1;
        }
        return __vmjs_x__;
      }
      f();
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_var_in_finally_block_is_hoisted() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        var out = "";
        out += __vmjs_x__;
        try {} finally { var __vmjs_x__ = 1; }
        out += "," + __vmjs_x__;
        return out;
      }
      f();
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "undefined,1");
  Ok(())
}

#[test]
fn compiled_var_in_with_block_assigns_to_var_binding_not_with_object() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        var x = 0;
        var o = { x: 2 };
        with (o) { var x = 1; }
        return o.x + "," + x;
      }
      f();
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "2,1");
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
fn compiled_with_block_instantiates_lexical_decls_for_tdz() -> Result<(), VmError> {
  // A block inside a `with` statement must still perform lexical declaration instantiation for
  // `let`/`const`, so the inner `let x` shadows the `with` binding immediately (TDZ).
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let o = {x: 1};
      with (o) { x; let x = 2; }
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
fn compiled_with_restores_outer_env_on_continue() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let i = 0;
      function getI() { return i; }
      let o = {i: 0};
      for (; i < 2; i++) {
        with (o) { continue; }
      }
      getI() + o.i * 10
    "#,
  )?;

  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // The loop update/condition must run in the outer environment (i=2, o.i=0). If the `with` env is
  // not restored after `continue`, the loop would iterate over o.i instead and return 20.
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
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
fn compiled_with_to_object_throws_for_undefined() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      let ok = 0;
      let u;
      try { with (u) { } } catch (e) { ok = 1; }
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
fn compiled_with_restores_outer_env_on_return_in_finally() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var o_global;
      var final_x;
      function f() {
        let x = 10;
        let o = { x: 2 };
        o_global = o;
        try {
          with (o) { return x; }
        } finally {
          // The `finally` must run in the outer lexical environment, not the `with` environment.
          // If the `with` env is not restored on `return`, this would assign to `o.x` instead.
          x = 30;
          final_x = x;
        }
      }
      var r = f();
      String(o_global.x) + ':' + String(final_x) + ':' + String(r)
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("2:30:2")?;
  assert!(result.same_value(Value::String(expected), scope.heap()));
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
fn compiled_try_catch_destructures_exception_value() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      try { throw {a: 1, b: 2}; } catch ({a}) { a + 1 }
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_try_catch_destructuring_default_observes_tdz() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // The default initializer `a = a` must refer to the *catch parameter* binding (which is in TDZ
  // during BindingInitialization), not an outer binding.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let a = 123;
      let ok = 0;
      try {
        try { throw {}; } catch ({a = a}) { ok = a; }
      } catch (e) {
        ok = 2;
      }
      ok
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_try_catch_tdz_shadowing_is_syntax_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let err = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      try { throw 1 } catch (e) { e; let e = 2; }
      'no'
    "#,
  )
  .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
  Ok(())
}

#[test]
fn compiled_try_catch_catches_throw() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ try { throw 1; } catch(e){ return e; } }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_try_catch_coerces_internal_not_callable() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Non-callable `Call` should surface as an internal `VmError::NotCallable`, which must be coerced
  // to a JS throw so `try/catch` can observe it.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      try { (0)(); } catch (e) { ok = 1; }
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_try_catch_coerces_internal_not_constructable() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Non-constructable `new` should surface as an internal `VmError::NotConstructable`, which must
  // be coerced to a JS throw so `try/catch` can observe it.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      try { new (0)(); } catch (e) { ok = 1; }
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
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
fn compiled_finally_overrides_return() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ try { return 1; } finally { return 2; } }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
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
fn compiled_try_catch_catches_typeerror() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ try { null.x; return 0; } catch(e){ return e instanceof TypeError; } }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
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
fn compiled_try_catch_coerces_internal_range_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 0;
      try { BigInt(1.1) } catch (e) { ok = (e.name === 'RangeError') ? 1 : 0; }
      ok
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_uncaught_range_error_is_coerced_to_throw_with_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", r#"BigInt(1.1)"#)?;
  let err = rt.exec_compiled_script(script).unwrap_err();

  let VmError::ThrowWithStack { value, .. } = err else {
    panic!("expected ThrowWithStack, got {err:?}");
  };
  let Value::Object(obj) = value else {
    panic!("expected thrown object, got {value:?}");
  };

  let range_error_proto = rt.realm().intrinsics().range_error_prototype();

  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(obj))?;
  assert_eq!(scope.heap().object_prototype(obj)?, Some(range_error_proto));
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
fn compiled_class_decl_tdz_shadows_outer_binding() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let C = 1;
      function f() {
        try { C; } catch(e) { return true; }
        return false;
        class C {}
      }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_function_block_let_tdz_throws_reference_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ { x; let x = 1; } }
      f()
    "#,
  )?;

  let err = rt.exec_compiled_script(script).unwrap_err();
  assert_thrown_is_reference_error(&rt, err)?;
  Ok(())
}

#[test]
fn compiled_function_block_let_shadowing_does_not_affect_outer() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ let x = 1; { let x = 2; } return x; }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_function_block_let_shadowing_before_init_throws_reference_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ let x = 1; { let y = x; let x = 2; return y; } }
      f()
    "#,
  )?;

  let err = rt.exec_compiled_script(script).unwrap_err();
  assert_thrown_is_reference_error(&rt, err)?;
  Ok(())
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
fn compiled_for_of_head_let_tdz_shadowing_throws() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x = 1;
      for (let x of [x]) {}
      'no'
    "#,
  )?;

  let err = rt.exec_compiled_script(script).unwrap_err();
  assert_thrown_is_reference_error(&rt, err)?;
  Ok(())
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
fn compiled_array_literal_holes_create_sparse_array() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let a = [1, , 3];
      (0 in a) && !(1 in a) && (2 in a);
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
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
      a[2];
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_array_literal_spread_from_string() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ return ['a', ...'bc'][2]; }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "c");
  Ok(())
}

#[test]
fn compiled_array_literal_holes_respect_fuel_budget() -> Result<(), VmError> {
  let holes = 10_000usize;
  let source = format!("function f() {{ return [{}]; }}", ",".repeat(holes));

  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(&mut heap, "test.js", source)?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  // The array literal contains no expressions, so the only per-element budgeting comes from
  // `eval_array_literal`'s per-hole ticks.
  vm.set_budget(Budget {
    fuel: Some(100),
    deadline: None,
    check_time_every: 1,
  });

  let res: Result<Value, VmError> = (|| {
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
    vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])
  })();

  realm.teardown(&mut heap);

  match res.unwrap_err() {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected OutOfFuel termination, got {other:?}"),
  }

  Ok(())
}

#[test]
fn compiled_array_literal_string_spread_respects_fuel_budget() -> Result<(), VmError> {
  let len = 500usize;
  let s = "a".repeat(len);
  let source = format!("function f() {{ return [...'{s}']; }}");

  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(&mut heap, "test.js", source)?;
  let f_body = find_function_body(&script, "f");
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  // Each element produced by string spread should consume budget even though the string iterator's
  // `next()` is native. The compiled HIR path charges one tick at the `next()` call boundary, and
  // an additional tick per spread element while appending to the array.
  vm.set_budget(Budget {
    // Between `N` and `2N` (plus a small constant), so this should only terminate if the array
    // literal's per-spread-element tick is present.
    fuel: Some(750),
    deadline: None,
    check_time_every: 1,
  });

  let res: Result<Value, VmError> = (|| {
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
    vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])
  })();

  realm.teardown(&mut heap);

  match res.unwrap_err() {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected OutOfFuel termination, got {other:?}"),
  }

  Ok(())
}

#[test]
fn compiled_array_literal_spread_step_error_does_not_close_iterator() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let it = {
        closed: 0,
        [Symbol.iterator]: function() { return this; },
        next: function() { throw 1; },
        return: function() {
          this.closed = this.closed + 1;
          return { done: true };
        },
      };
      try { [...it]; } catch (e) {}
      it.closed
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(0.0));
  Ok(())
}

#[test]
fn compiled_array_literal_spread_value_error_does_not_close_iterator() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let it = {
        closed: 0,
        [Symbol.iterator]: function() { return this; },
        next: function() {
          return {
            get done() { return false; },
            get value() { throw 1; },
          };
        },
        return: function() {
          this.closed = this.closed + 1;
          return { done: true };
        },
      };
      try { [...it]; } catch (e) {}
      it.closed
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(0.0));
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
fn compiled_switch_labeled_break_preserves_value() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // The `break label` should propagate out of the switch with the running completion value.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      label: switch (0) {
        case 0: 1; break label;
      }
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
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
fn compiled_switch_fallthrough_and_break() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(x){
        switch(x){
          case 1: 10;
          case 2: return 20;
          default: return 30;
        }
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
  assert_eq!(r1, Value::Number(20.0));

  let r3 = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Number(3.0)],
  )?;
  assert_eq!(r3, Value::Number(30.0));
  Ok(())
}

#[test]
fn compiled_switch_break_exits_only_switch() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f(){
        let i = 0;
        let out = 0;
        while (i < 3) {
          switch(i){
            case 0: out = out + 1; break;
            case 1: out = out + 10; break;
            default: out = out + 100; break;
          }
          i = i + 1;
        }
        return out;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(111.0));
  Ok(())
}

#[test]
fn compiled_switch_case_matching_uses_strict_equality() -> Result<(), VmError> {
  // `switch` case selection uses strict equality (`===`), not abstract equality (`==`).
  //
  // If it used `==`, `"1"` would match `case 1` and return 10.
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(x){
        switch (x) {
          case 1: return 10;
          case "1": return 20;
          default: return 30;
        }
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

  let arg = scope.alloc_string("1")?;
  let r_str = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::String(arg)],
  )?;
  assert_eq!(r_str, Value::Number(20.0));

  let r_num = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Number(1.0)],
  )?;
  assert_eq!(r_num, Value::Number(10.0));
  Ok(())
}

#[test]
fn compiled_switch_continue_targets_outer_loop() -> Result<(), VmError> {
  // `continue` inside a switch statement should target the nearest enclosing loop, and must not be
  // consumed by the switch itself.
  let result = compile_and_call0(
    r#"
      function f(){
        let i = 0;
        let out = 0;
        while (i < 3) {
          switch(i){
            case 0: out = out + 1; i = i + 1; continue;
            case 1: out = out + 10; i = i + 1; continue;
            default: out = out + 100; i = i + 1; continue;
          }
          // If `continue` is incorrectly consumed by the switch, control reaches here.
          out = out + 1000;
        }
        return out;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(111.0));
  Ok(())
}

#[test]
fn compiled_switch_function_labeled_break_exits_outer_statement() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f(x) {
        let y = 0;
        outer: {
          switch (x) {
            case 1:
              y = 1;
              break outer;
            default:
              y = 2;
          }
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

  let r1 = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Number(1.0)],
  )?;
  assert_eq!(r1, Value::Number(1.0));

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
fn compiled_switch_function_default_in_middle_falls_through() -> Result<(), VmError> {
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
          default:
            y = 2;
          case 3:
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

  let r1 = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Number(1.0)],
  )?;
  assert_eq!(r1, Value::Number(1.0));

  let r3 = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Number(3.0)],
  )?;
  assert_eq!(r3, Value::Number(3.0));

  let r9 = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Number(9.0)],
  )?;
  // No match => start at `default`, then fall through to the subsequent `case 3`.
  assert_eq!(r9, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_switch_function_discriminant_evaluated_once() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let i = 0;
        let y = 0;
        // The discriminant must be evaluated once even when the first case does not match.
        switch (i++) {
          case 1:
            y = 10;
            break;
          case 0:
            y = 20;
            break;
          default:
            y = 30;
        }
        return i * 100 + y;
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
  assert_eq!(result, Value::Number(120.0));
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
fn compiled_member_compound_assignment_add_assign_executes_in_function() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "test.js",
    r#"
      function f() {
        let o = { x: 1 };
        let r = (o.x += 2);
        return r * 10 + o.x;
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

  // `o.x += 2` returns 3 and leaves `o.x == 3`, so `r*10 + o.x == 33`.
  let result = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
  assert_eq!(result, Value::Number(33.0));
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
fn compiled_bigint_shift_left_assign_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint(
    r#"
      let x = 1n;
      x <<= 2n;
      x
    "#,
    4,
  )
}

#[test]
fn compiled_bigint_shift_left_assign_with_negative_count_shifts_right() -> Result<(), VmError> {
  // Match interpreter semantics: `x <<= -y` is `x >>= y`.
  assert_compiled_script_bigint(
    r#"
      let x = 8n;
      x <<= -1n;
      x
    "#,
    4,
  )
}

#[test]
fn compiled_bigint_shift_right_assign_with_negative_count_shifts_left() -> Result<(), VmError> {
  assert_compiled_script_bigint(
    r#"
      let x = 4n;
      x >>= -1n;
      x
    "#,
    8,
  )
}

#[test]
fn compiled_bigint_bitwise_and_assign_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint(
    r#"
      let x = 5n;
      x &= 3n;
      x
    "#,
    1,
  )
}

#[test]
fn compiled_bigint_bitwise_xor_assign_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint(
    r#"
      let x = 1n;
      x ^= 3n;
      x
    "#,
    2,
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
fn compiled_shift_left_assign_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x = 5;
      x <<= 1;
      x
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(10.0));
  Ok(())
}

#[test]
fn compiled_shift_right_unsigned_assign_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x = -1;
      x >>>= 1;
      x
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2147483647.0));
  Ok(())
}

#[test]
fn compiled_bigint_shift_right_unsigned_assign_throws_type_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let msg = 'no';
      try {
        let x = 1n;
        x >>>= 1n;
      } catch (e) {
        msg = e.message;
      }
      msg
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(
    rt.heap().get_string(s)?.to_utf8_lossy(),
    "BigInt does not support unsigned right shift"
  );
  Ok(())
}

#[test]
fn compiled_bigint_shift_right_assign_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint(
    r#"
      let x = 7n;
      x >>= 1n;
      x
    "#,
    3,
  )
}

#[test]
fn compiled_bitwise_and_assign_executes() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x = 5;
      x &= 3;
      x
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_bigint_bitwise_or_assign_executes() -> Result<(), VmError> {
  assert_compiled_script_bigint(
    r#"
      let x = 1n;
      x |= 2n;
      x
    "#,
    3,
  )
}

#[test]
fn compiled_logical_or_assign_short_circuits_rhs() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x = 1;
      let y = 0;
      x ||= (y = 1);
      y
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(0.0));
  Ok(())
}

#[test]
fn compiled_logical_and_assign_short_circuits_rhs() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x = 0;
      let y = 0;
      x &&= (y = 1);
      y
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(0.0));
  Ok(())
}

#[test]
fn compiled_nullish_assign_short_circuits_rhs() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let x = 0;
      let y = 0;
      x ??= (y = 1);
      y
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(0.0));
  Ok(())
}

#[test]
fn compiled_logical_assignment_infers_function_names() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let o = {};
      o.f ||= function() {};
      o.a ||= () => {};
      o.c ||= class {};
      o.g = 1;
      o.g &&= function() {};

      let x;
      x ||= function() {};
      let y = true;
      y &&= function() {};
      let z;
      z ??= () => {};
      let w;
      w ??= class {};

      o.f.name + '|' + o.a.name + '|' + o.c.name + '|' + o.g.name + '|' +
        x.name + '|' + y.name + '|' + z.name + '|' + w.name
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;

  let mut scope = rt.heap.scope();
  scope.push_root(result)?;
  let expected = scope.alloc_string("f|a|c|g|x|y|z|w")?;
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
fn compiled_delete_member_in_function_removes_property() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        let o = { x: 1 };
        delete o.x;
        return ("x" in o);
      }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(false));
  Ok(())
}

#[test]
fn compiled_delete_identifier_returns_false_for_var_binding() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        var x = 1;
        return delete x;
      }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(false));
  Ok(())
}

#[test]
fn compiled_delete_missing_global_returns_true() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        return delete notDefined;
      }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_delete_identifier_deletes_with_binding_property() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let o = { x: 1 };
      let r;
      with (o) { r = delete x; }
      String(r) + "|" + ("x" in o)
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;

  let scope = rt.heap_mut().scope();
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(scope.heap().get_string(s)?.to_utf8_lossy(), "true|false");
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
fn compiled_delete_optional_chain_computed_member_does_not_evaluate_key_when_nullish() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let side = 0;
      let o = null;
      delete o?.[side = 1];
      side === 0
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_optional_chain_continuation_short_circuits_to_undefined() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let a = null;
      a?.b.c
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_optional_chain_continuation_propagates_through_function_call() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(a) { return a?.b.c; }
      f(null) === undefined && f({ b: { c: 1 } }) === 1
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_optional_chain_continuation_skips_computed_key_side_effects() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(a) {
        let hit = 0;
        function key() { hit = hit + 1; return 'c'; }
        a?.b[key()];
        return hit;
      }
      f(null)
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(0.0));
  Ok(())
}

#[test]
fn compiled_optional_chain_continuation_skips_call_args_side_effects() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(a) {
        let hit = 0;
        function arg() { hit = hit + 1; return 1; }
        a?.b.c(arg());
        return hit;
      }
      f(null)
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(0.0));
  Ok(())
}

#[test]
fn compiled_optional_chain_call_continuation_short_circuits_to_undefined() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let a = null;
      a?.b()()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_delete_optional_chain_continuation_short_circuits_and_skips_key() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let a = null;
      let ok = 0;
      let r = delete a?.b[ok = 1];
      r === true && ok === 0
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
fn compiled_instanceof_rhs_not_callable_throws() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", "({} instanceof {})")?;
  let err = rt.exec_compiled_script(script).unwrap_err();
  match err {
    VmError::ThrowWithStack { .. } | VmError::Throw(_) | VmError::TypeError(_) => Ok(()),
    other => panic!("expected TypeError, got {other:?}"),
  }
}

#[test]
fn compiled_instanceof_custom_has_instance() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let C = { [Symbol.hasInstance](x) { return true; } };
      ({} instanceof C)
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_instanceof_has_instance_not_callable_throws() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let C = { [Symbol.hasInstance]: 1 };
      ({} instanceof C)
    "#,
  )?;
  let err = rt.exec_compiled_script(script).unwrap_err();
  match err {
    VmError::ThrowWithStack { .. } | VmError::Throw(_) | VmError::TypeError(_) => Ok(()),
    other => panic!("expected TypeError, got {other:?}"),
  }
}

#[test]
fn compiled_instanceof_function_prototype_not_object_throws() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function C() {}
      C.prototype = 1;
      ({} instanceof C)
    "#,
  )?;
  let err = rt.exec_compiled_script(script).unwrap_err();
  match err {
    VmError::ThrowWithStack { .. } | VmError::Throw(_) | VmError::TypeError(_) => Ok(()),
    other => panic!("expected TypeError, got {other:?}"),
  }
}

#[test]
fn compiled_instanceof_uses_proxy_get_trap_for_has_instance() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Split-borrow the runtime so we can access both VM (for native call registration) and Heap (for
  // compilation/allocation) without borrow checker gymnastics.
  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();

  let script = CompiledScript::compile_script(
    heap,
    "test.js",
    r#"
      function f(o, C) { return o instanceof C; }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let get_trap_id = vm.register_native_call(proxy_get_trap_return_target)?;
  let has_instance_id = vm.register_native_call(native_has_instance_returns_true)?;

  let mut scope = heap.scope();

  // Create `@@hasInstance` function (native, returns true).
  let has_instance_name = scope.alloc_string("hasInstance")?;
  scope.push_root(Value::String(has_instance_name))?;
  let has_instance_fn = scope.alloc_native_function(has_instance_id, None, has_instance_name, 1)?;
  scope.push_root(Value::Object(has_instance_fn))?;

  // Proxy handler: { get: <native get trap> }, where the trap returns the Proxy target object (the
  // `has_instance_fn` we set below).
  let handler = scope.alloc_object()?;
  scope.push_root(Value::Object(handler))?;

  let get_name = scope.alloc_string("get")?;
  scope.push_root(Value::String(get_name))?;
  let get_fn = scope.alloc_native_function(get_trap_id, None, get_name, 3)?;
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

  // Set the Proxy target to the custom `@@hasInstance` method. The get trap returns its target for
  // all property keys, so `GetMethod(proxy, @@hasInstance)` yields `has_instance_fn`.
  let proxy = scope.alloc_proxy(Some(has_instance_fn), Some(handler))?;
  scope.push_root(Value::Object(proxy))?;

  let lhs = scope.alloc_object()?;
  scope.push_root(Value::Object(lhs))?;

  let f_name = scope.alloc_string("f")?;
  scope.push_root(Value::String(f_name))?;
  let f = scope.alloc_user_function(
    CompiledFunctionRef {
      script,
      body: f_body,
    },
    f_name,
    2,
  )?;
  scope.push_root(Value::Object(f))?;

  // If `instanceof` uses `GetMethod(C, @@hasInstance)` with proper Proxy semantics, the Proxy `get`
  // trap will supply a custom `@@hasInstance` method and `lhs instanceof proxy` evaluates to true.
  //
  // If the compiled engine bypasses Proxy traps, it will fall back to
  // `Function.prototype[@@hasInstance]` (OrdinaryHasInstance) and return false because `lhs` is not
  // in `has_instance_fn.prototype`'s chain.
  let result = vm.call_without_host(
    &mut scope,
    Value::Object(f),
    Value::Undefined,
    &[Value::Object(lhs), Value::Object(proxy)],
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_instanceof_bound_function_delegation() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function C() {}
      let B = C.bind(null);
      let o = Object.create(C.prototype);
      o instanceof B
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
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
fn compiled_for_in_enumerates_keys() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let out = "";
      for (let k in ({a: 1, b: 2})) {
        out += k;
      }
      out.includes("a") && out.includes("b")
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_for_in_null_rhs_is_empty_iteration() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ran = false;
      for (let k in null) { ran = true; }
      ran
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(false));
  Ok(())
}

#[test]
fn compiled_for_in_undefined_rhs_is_empty_iteration() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ran = false;
      for (let k in undefined) { ran = true; }
      ran
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(false));
  Ok(())
}

#[test]
fn compiled_while_completion_value_overrides_previous_value() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // `while (false) {}` returns a normal completion with value `undefined`, which overrides the
  // preceding statement's completion value.
  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", "1; while (false) {}")?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_while_break_preserves_completion_value() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // `break` should carry the running completion value out of the loop.
  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", "while (true) { 1; break; }")?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_for_of_completion_value_is_last_value() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", "for (let x of [0]) { 1; }")?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_for_in_completion_value_is_last_value() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", "for (let k in ({a: 1})) { 1; }")?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_for_of_labeled_break_empty_value_becomes_undefined() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // A labelled `break` exiting an iteration statement should update its empty value to the loop's
  // running completion value (`undefined` here), which then overrides the preceding statement's
  // completion value.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    "1; label: for (let x of [0]) { break label; }",
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_for_of_sums_array() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let s = 0;
      for (let x of [1, 2, 3]) {
        s += x;
      }
      s
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(6.0));
  Ok(())
}

#[test]
fn compiled_for_of_break_closes_iterator() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let it = {
        i: 0,
        closed: 0,
        [Symbol.iterator]: function() { return this; },
        next: function() {
          this.i = this.i + 1;
          return { value: 1, done: this.i > 1 };
        },
        return: function() {
          this.closed = this.closed + 1;
          return { done: true };
        },
      };
      for (let x of it) { break; }
      it.closed
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_for_of_step_error_does_not_close_iterator() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let it = {
        closed: 0,
        [Symbol.iterator]: function() { return this; },
        next: function() { throw 1; },
        return: function() {
          this.closed = this.closed + 1;
          return { done: true };
        },
      };
      try {
        for (let x of it) {}
      } catch (e) {}
      it.closed
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(0.0));
  Ok(())
}

#[test]
fn compiled_for_of_body_throw_closes_iterator() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let it = {
        i: 0,
        closed: 0,
        [Symbol.iterator]: function() { return this; },
        next: function() {
          this.i = this.i + 1;
          return { value: 1, done: this.i > 1 };
        },
        return: function() {
          this.closed = this.closed + 1;
          return { done: true };
        },
      };
      try {
        for (let x of it) { throw 1; }
      } catch (e) {}
      it.closed
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_for_of_labeled_continue_to_outer_closes_iterator() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let it = {
        i: 0,
        closed: 0,
        [Symbol.iterator]: function() { return this; },
        next: function() {
          this.i = this.i + 1;
          return { value: 1, done: this.i > 1 };
        },
        return: function() {
          this.closed = this.closed + 1;
          return { done: true };
        },
      };
      let ran = false;
      outer: while (!ran) {
        for (let x of it) {
          ran = true;
          continue outer;
        }
      }
      it.closed
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_for_of_return_closes_iterator_before_finally() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        let it = {
          i: 0,
          closed: 0,
          [Symbol.iterator]: function() { return this; },
          next: function() {
            this.i = this.i + 1;
            return { value: 1, done: this.i > 1 };
          },
          return: function() {
            this.closed = this.closed + 1;
            return { done: true };
          },
        };
        let r = { before: 0, after: 0 };
        try {
          for (let x of it) {
            r.before = it.closed;
            return r;
          }
        } finally {
          r.after = it.closed;
        }
      }
      let r = f();
      r.before * 10 + r.after
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  // `r.before` captures the value before IteratorClose runs, while `r.after` is observed in
  // `finally` after the iterator has been closed.
  assert_eq!(result, Value::Number(1.0));
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
fn compiled_while_completion_value_is_undefined() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      1;
      while (false) {}
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_try_completion_value_is_undefined() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      1;
      try {} finally {}
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_while_break_preserves_last_value() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      1;
      while (true) {
        2;
        break;
      }
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_labeled_break_preserves_last_value() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      label: {
        1;
        break label;
      }
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_try_resets_labeled_break_completion_value_to_undefined() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      label: {
        1;
        try { break label; } finally {}
      }
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_while_continue_preserves_last_value() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let i = 0;
      while (i < 2) {
        i = i + 1;
        i;
        continue;
      }
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_for_in_skips_null_rhs() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = 1;
      for (let k in null) { ok = 0; }
      ok
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
fn compiled_direct_eval_updates_local_let() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ let x = 1; eval('x = 2'); return x; }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_direct_eval_var_decl_conflicts_with_let() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){
        let x = 1;
        try { eval('var x = 2'); } catch (e) { return e.name; }
        return "did not throw";
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "SyntaxError");
  Ok(())
}

#[test]
fn compiled_missing_initializer_in_destructuring_decl_is_syntax_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Destructuring `let` declarations require an initializer (early error).
  let script = match CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      if (false) { let {x}; }
    "#,
  ) {
    Ok(script) => script,
    Err(VmError::Syntax(_)) => return Ok(()),
    Err(other) => return Err(other),
  };

  let err = rt.exec_compiled_script(script).unwrap_err();
  match err {
    VmError::Syntax(_) => Ok(()),
    other => panic!("expected syntax error, got {other:?}"),
  }
}

#[test]
fn compiled_missing_initializer_in_const_decl_is_syntax_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // `const` declarations require an initializer (early error).
  let script = match CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      if (false) { const x; }
    "#,
  ) {
    Ok(script) => script,
    Err(VmError::Syntax(_)) => return Ok(()),
    Err(other) => return Err(other),
  };

  let err = rt.exec_compiled_script(script).unwrap_err();
  match err {
    VmError::Syntax(_) => Ok(()),
    other => panic!("expected syntax error, got {other:?}"),
  }
}

#[test]
fn compiled_missing_initializer_in_function_body_is_syntax_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Even when the invalid declaration appears inside a function body, the runtime should still
  // surface it as a syntax error (not a runtime TypeError) when the function is invoked.
  let script = match CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ if (false) { let {x}; } }
      f()
    "#,
  ) {
    Ok(script) => script,
    Err(VmError::Syntax(_)) => return Ok(()),
    Err(other) => return Err(other),
  };

  let err = rt.exec_compiled_script(script).unwrap_err();
  match err {
    VmError::Syntax(_) => Ok(()),
    other => panic!("expected syntax error, got {other:?}"),
  }
}

#[test]
fn compiled_indirect_eval_does_not_see_local_let() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ let x = 1; (0, eval)('x = 2'); return x; }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_parenthesized_eval_is_indirect() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ let x = 1; (eval)('x = 2'); return x; }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_sloppy_direct_eval_var_decl_leaks_to_caller() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ eval('var x = 1'); return x; }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_strict_direct_eval_var_decl_does_not_leak_to_caller() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ 'use strict'; eval('var x = 1'); return typeof x; }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "undefined");
  Ok(())
}

#[test]
fn compiled_direct_eval_source_use_strict_var_decl_does_not_leak_to_caller() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ eval("'use strict'; var x = 1"); return typeof x; }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "undefined");
  Ok(())
}

#[test]
fn compiled_sloppy_direct_eval_function_decl_leaks_to_caller() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ eval('function g(){ return 1; }'); return g(); }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_strict_direct_eval_function_decl_does_not_leak_to_caller() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ 'use strict'; eval('function g(){ return 1; }'); return typeof g; }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "undefined");
  Ok(())
}

#[test]
fn compiled_global_direct_eval_var_decl_is_deletable() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      eval('var __eval_global_deletable_var = 1');
      delete __eval_global_deletable_var;
      typeof __eval_global_deletable_var
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "undefined");
  Ok(())
}

#[test]
fn compiled_global_direct_eval_function_decl_is_deletable() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      eval('function __eval_global_deletable_fn(){ return 1; }');
      delete __eval_global_deletable_fn;
      typeof __eval_global_deletable_fn
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "undefined");
  Ok(())
}

#[test]
fn compiled_parenthesized_use_strict_is_not_directive_in_function() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ ('use strict'); x = 1; return x; }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_parenthesized_use_strict_with_comment_is_not_directive_in_function() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Ensure directive-prologue detection skips trivia correctly. A parenthesized string literal is
  // never a directive, even if there are comments between the literal token and the closing `)`.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ ('use strict'/*comment*/); x = 1; return x; }
      f()
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_parenthesized_use_strict_is_not_directive_in_script() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      ('use strict');
      x = 1;
      x
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_parenthesized_use_strict_with_line_comment_is_not_directive_in_script() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      ('use strict'//comment
      );
      x = 1;
      x
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
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
fn compiled_direct_eval_inherits_strictness_from_caller() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ "use strict"; eval("x = 1"); }
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
fn compiled_direct_eval_use_strict_directive_in_source_makes_eval_strict() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ eval("'use strict'; x = 1"); }
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
fn compiled_direct_eval_var_decl_is_function_scoped_in_sloppy_function() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        eval("var x = 1");
        return (typeof x) + "," + (typeof this.x);
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "number,undefined");
  Ok(())
}

#[test]
fn compiled_optional_chain_eval_is_indirect() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ let x = 1; eval?.('x = 2'); return x; }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
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
fn compiled_arguments_length_counts_all_passed_arguments() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(a){ return arguments.length; }
      f(1, 2, 3)
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
fn compiled_first_param_default_initializer_can_read_arguments_object() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // The first parameter default initializer runs before any parameter binding is initialized.
  // It must still be able to read the `arguments` object, including indices that are beyond the
  // formal parameter list.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(a = arguments[2], b){ return a; }
      f(undefined, 1, 7)
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(7.0));
  Ok(())
}

#[test]
fn compiled_strict_arguments_callee_is_poison_pill() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){
        'use strict';
        try { arguments.callee; } catch (e) { return e.name; }
        return "did not throw";
      }
      f()
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "TypeError");
  Ok(())
}

#[test]
fn compiled_sloppy_duplicate_parameter_names_last_wins() -> Result<(), VmError> {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(a, a){ return a; }
      f(1, 2)
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
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
fn compiled_user_function_constructor_has_prototype_object() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function C() {}
      typeof C.prototype
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "object");
  Ok(())
}

#[test]
fn compiled_method_function_does_not_have_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      ({m(){}}).m.prototype
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_user_function_prototype_is_used_for_instances() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function C() {}
      C.prototype.y = 1;
      (new C()).y
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_user_function_prototype_constructor_points_to_function() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function C() {}
      C.prototype.constructor === C
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_arrow_function_does_not_have_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      (() => {}).prototype
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Undefined);
  Ok(())
}

#[test]
fn compiled_arrow_function_is_not_constructable_throws_type_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let ok = true;
      try { new (() => {})(); ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }
      ok
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_object_literal_accessor_functions_do_not_have_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let o = { get x() { return 1; }, set x(v) {} };
      let d = Object.getOwnPropertyDescriptor(o, "x");
      d.get.prototype === undefined && d.set.prototype === undefined
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_object_literal_accessor_functions_are_not_constructors() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let o = { get x() { return 1; }, set x(v) {} };
      let d = Object.getOwnPropertyDescriptor(o, "x");
      let ok = true;
      try { new (d.get)(); ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }
      try { new (d.set)(); ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }
      ok
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
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
fn compiled_object_destructuring_rest_excludes_named_properties() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let src = { x: 1, y: 2 };
      let { x, ...rest } = src;
      !("x" in rest) && ("y" in rest) && rest.y === 2 && x === 1
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_object_destructuring_rest_excludes_symbol_properties() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let sym = Symbol("s");
      let src = { a: 2 };
      src[sym] = 1;
      let { [sym]: x, ...rest } = src;
      x === 1 && rest.a === 2 && Object.getOwnPropertySymbols(rest).length === 0
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_object_destructuring_rest_copies_symbol_properties() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let sym = Symbol("s");
      let src = {};
      src[sym] = 1;
      let { ...rest } = src;
      rest[sym]
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_object_destructuring_rest_does_not_trigger_proto_setter() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Object rest uses CopyDataProperties/CreateDataProperty, so copying an own `"__proto__"` data
  // property must not mutate the rest object's prototype.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let proto = { y: 1 };
      let src = {};
      Object.defineProperty(src, "__proto__", { value: proto, enumerable: true, configurable: true, writable: true });
      let { ...rest } = src;
      let desc = Object.getOwnPropertyDescriptor(rest, "__proto__");
      Object.getPrototypeOf(rest) === Object.prototype &&
        desc !== undefined &&
        desc.value === proto
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_object_destructuring_rest_skips_non_enumerable_getters() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let log = "";
      let src = {};
      Object.defineProperty(src, "x", { get() { log += "x"; return 1; }, enumerable: false, configurable: true });
      let { ...rest } = src;
      log === "" && Object.getOwnPropertyDescriptor(rest, "x") === undefined
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
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

#[test]
fn compiled_named_function_expr_name_binding_is_immutable() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        let g = function fact() {
          fact = 1;
          return 0;
        };
        try {
          g();
          return false;
        } catch (e) {
          return e instanceof TypeError;
        }
      }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_named_function_expr_name_binding_does_not_leak_to_outer_scope() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        let g = function fact() { return 1; };
        return typeof fact;
      }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "undefined");
  Ok(())
}

#[test]
fn compiled_var_decl_object_destructuring_default() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f(){ let {a,b=2} = {a:1}; return a + b; }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_var_decl_array_destructuring_default() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f(){ let [a=1] = []; return a; }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_var_decl_object_destructuring_alias_executes() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f(){ let {x, y: z} = ({x: 1, y: 2}); return x + z; }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_var_decl_array_destructuring_elision() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f(){ let [a,,b] = [1,2,3]; return a + b; }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(4.0));
  Ok(())
}

#[test]
fn compiled_assignment_object_destructuring() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f(){ let a=0; ({a} = {a:5}); return a; }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(5.0));
  Ok(())
}

#[test]
fn compiled_destructuring_computed_member_target_delays_to_property_key_conversion() -> Result<(), VmError> {
  // Mirrors test262:
  // `language/expressions/assignment/destructuring/keyed-destructuring-property-reference-target-evaluation-order.js`
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let log = [];

      function source() {
        log.push("source");
        return {
          get p() { log.push("get"); }
        };
      }

      function target() {
        log.push("target");
        return {
          set q(v) { log.push("set"); }
        };
      }

      function sourceKey() {
        log.push("source-key");
        return { toString() { log.push("source-key-tostring"); return "p"; } };
      }

      function targetKey() {
        log.push("target-key");
        return { toString() { log.push("target-key-tostring"); return "q"; } };
      }

      ({ [sourceKey()]: target()[targetKey()] } = source());
      log.join("|")
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(
    rt.heap().get_string(s)?.to_utf8_lossy(),
    "source|source-key|source-key-tostring|target|target-key|get|target-key-tostring|set"
  );
  Ok(())
}

#[test]
fn compiled_destructuring_computed_member_target_delays_to_property_key_past_default_eval() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let log = [];

      function source() {
        log.push("source");
        return {
          get p() { log.push("get"); return undefined; }
        };
      }

      function target() {
        log.push("target");
        return {
          set q(v) { log.push("set"); }
        };
      }

      function targetKey() {
        log.push("target-key");
        return { toString() { log.push("target-key-tostring"); return "q"; } };
      }

      function def() { log.push("default"); return 1; }

      ({ p: target()[targetKey()] = def() } = source());
      log.join("|")
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(
    rt.heap().get_string(s)?.to_utf8_lossy(),
    "source|target|target-key|get|default|target-key-tostring|set"
  );
  Ok(())
}

#[test]
fn compiled_array_destructuring_computed_member_target_delays_to_property_key_conversion() -> Result<(), VmError> {
  // Mirrors test262:
  // `language/expressions/assignment/destructuring/iterator-destructuring-property-reference-target-evaluation-order.js`
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let log = [];

      function source() {
        log.push("source");
        let iterator = {
          next() {
            log.push("iterator-step");
            return {
              get done() { log.push("iterator-done"); return true; },
              get value() { log.push("iterator-value"); }
            };
          },
        };
        let src = {};
        src[Symbol.iterator] = function() {
          log.push("iterator");
          return iterator;
        };
        return src;
      }

      function target() {
        log.push("target");
        return {
          set q(v) { log.push("set"); }
        };
      }

      function targetKey() {
        log.push("target-key");
        return { toString() { log.push("target-key-tostring"); return "q"; } };
      }

      ([target()[targetKey()]] = source());
      log.join("|")
    "#,
  )?;
  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string, got {result:?}");
  };
  assert_eq!(
    rt.heap().get_string(s)?.to_utf8_lossy(),
    "source|iterator|target|target-key|iterator-step|iterator-done|target-key-tostring|set"
  );
  Ok(())
}

#[test]
fn compiled_array_destructuring_rest() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f(){ let [a,...rest] = [1,2,3]; return rest.length; }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_object_destructuring_rest() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f(){ let {a, ...rest} = {a:1,b:2,c:3}; return ("b" in rest) && !("a" in rest); }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_new_constructs_user_function() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function C(x) { this.x = x; }
      function f() { return new C(7).x; }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(7.0));
  Ok(())
}

#[test]
fn compiled_new_with_spread_args() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function C(x) { this.x = x; }
      function f() { return new C(...[7]).x; }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(7.0));
  Ok(())
}

#[test]
fn compiled_new_parenthesized_call_evaluates_expression_first() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // `new (make())` must first evaluate the parenthesized call expression `make()` (producing a
  // constructor), then `[[Construct]]` the *result* with no arguments. This differs from `new make()`.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function make() { return function C(){ this.x = 7; }; }
      function f() { return (new (make())).x; }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(7.0));
  Ok(())
}

#[test]
fn compiled_call_with_spread_args() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        return Math.max(...[1, 2, 3]);
      }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_member_access_boxes_primitives() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ return "abc".length; }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_method_call_uses_base_as_this() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ return "abc".slice(1); }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "bc");
  Ok(())
}

#[test]
fn compiled_object_literal_has_object_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f(){ return typeof ({}).toString; }
      f();
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "function");
  Ok(())
}

#[test]
fn compiled_strict_assignment_to_primitive_throws() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use strict";
      ("a").x = 1;
    "#,
  )?;

  let err = rt.exec_compiled_script(script).unwrap_err();
  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));
  let Value::Object(thrown_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };

  let type_error_proto = rt.realm().intrinsics().type_error_prototype();
  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(thrown_obj))?;
  let thrown_proto = scope.heap().object_prototype(thrown_obj)?;
  assert_eq!(thrown_proto, Some(type_error_proto));
  Ok(())
}

#[test]
fn compiled_catch_object_destructuring_executes() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        try { throw {x: 1}; } catch ({x}) { return x; }
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_catch_object_destructuring_default_observes_tdz() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let x = 1;
        let threw = false;
        try {
          try { throw {}; } catch ({x = x}) {}
        } catch (e) {
          threw = true;
        }
        return threw;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_for_loop_lexical_init_object_destructuring_executes() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let out = 0;
        for (let {x} = {x: 2}; ; ) { out = x; break; }
        return out;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_for_of_head_let_object_destructuring_executes() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let out = 0;
        for (let {x} of [{x: 3}]) { out = x; }
        return out;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_for_of_head_let_destructuring_default_observes_tdz() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let x = 1;
        let threw = false;
        try {
          for (let {x = x} of [{}]) {}
        } catch (e) {
          threw = true;
        }
        return threw;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_for_of_head_let_shadows_rhs_in_tdz() -> Result<(), VmError> {
  // For-in/of loops with lexical declarations create a TDZ binding for the loop variable *before*
  // evaluating the RHS. This ensures `for (let x of x)` throws a ReferenceError rather than
  // iterating an outer `x`.
  let result = compile_and_call0(
    r#"
      function f() {
        let x = [1];
        let threw = false;
        try {
          for (let x of x) {}
        } catch (e) {
          threw = true;
        }
        return threw;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_for_in_head_let_shadows_rhs_in_tdz() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let x = {a: 1};
        let threw = false;
        try {
          for (let x in x) {}
        } catch (e) {
          threw = true;
        }
        return threw;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_for_loop_lexical_init_destructuring_default_observes_tdz() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let x = 1;
        let threw = false;
        try {
          for (let {x = x} = {}; ; ) break;
        } catch (e) {
          threw = true;
        }
        return threw;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_for_of_head_assignment_object_destructuring_executes() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let x = 0;
        for ({x} of [{x: 4}]) {}
        return x;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(4.0));
  Ok(())
}

#[test]
fn compiled_for_of_head_let_object_destructuring_closure_captures_each_iteration_value() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let fs = [];
        for (let {x} of [{x: 1}, {x: 2}]) {
          fs.push(function() { return x; });
        }
        return fs[0]() * 10 + fs[1]();
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(12.0));
  Ok(())
}

#[test]
fn compiled_for_in_head_let_object_destructuring_closure_captures_each_iteration_value() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let fs = [];
        for (let {length: x} in ({a: 1, bb: 2})) {
          fs.push(function() { return x; });
        }
        return fs[0]() * 10 + fs[1]();
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(12.0));
  Ok(())
}

#[test]
fn compiled_for_of_head_const_object_destructuring_closure_captures_each_iteration_value() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let fs = [];
        for (const {x} of [{x: 1}, {x: 2}]) {
          fs.push(function() { return x; });
        }
        return fs[0]() * 10 + fs[1]();
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(12.0));
  Ok(())
}

#[test]
fn compiled_for_in_head_const_object_destructuring_closure_captures_each_iteration_value() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        let fs = [];
        for (const {length: x} in ({a: 1, bb: 2})) {
          fs.push(function() { return x; });
        }
        return fs[0]() * 10 + fs[1]();
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(12.0));
  Ok(())
}

#[test]
fn compiled_optional_member_callee_parentheses_do_not_short_circuit_call() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let log = '';
      function key(){ log = log + 'k'; return 'm'; }
      function arg(){ log = log + 'a'; }
      let o = null;
      (o?.[key()])(arg());
    "#,
  )?;

  // The script should throw because the outer call is not part of the optional chain.
  let _ = rt.exec_compiled_script(script).unwrap_err();

  // The optional member access should skip `key()`, but the outer call should still evaluate
  // `arg()` before throwing.
  let log = rt.exec_script("log")?;
  let Value::String(s) = log else {
    panic!("expected string result, got {log:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "a");
  Ok(())
}

#[test]
fn compiled_optional_chain_member_continuation_short_circuits() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let o = null;
      let ok = 0;
      try { o?.x.y; ok = 1; } catch (e) { ok = 2; }
      ok
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_optional_chain_member_continuation_does_not_evaluate_computed_key() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let side = 0;
      let o = null;
      let ok = 0;
      try { o?.x[side = 1]; ok = 1; } catch (e) { ok = 2; }
      ok * 10 + side
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(10.0));
  Ok(())
}

#[test]
fn compiled_optional_chain_call_continuation_does_not_evaluate_computed_key() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let side = 0;
      let o = null;
      let ok = 0;
      try { o?.m()[side = 1]; ok = 1; } catch (e) { ok = 2; }
      ok * 10 + side
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(10.0));
  Ok(())
}

#[test]
fn compiled_parenthesized_optional_chain_breaks_propagation() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let o = null;
      let ok = 0;
      try { (o?.x).y; ok = 1; } catch (e) { ok = 2; }
      ok
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_optional_chaining_call_short_circuits_args() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      let log = '';
      function arg(){ log = log + 'a'; }
      let o = null;
      o?.m(arg());
      log
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  assert_eq!(rt.heap().get_string(s)?.to_utf8_lossy(), "");
  Ok(())
}

#[test]
fn compiled_import_meta_outside_module_is_syntax_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let err = CompiledScript::compile_script(rt.heap_mut(), "test.js", "import.meta;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
  Ok(())
}

#[test]
fn compiled_import_call_requires_module_graph() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(rt.heap_mut(), "test.js", "import('x');")?;

  // `JsRuntime` installs a runtime-owned module graph by default; clear it so dynamic import takes
  // the "missing module graph" error path.
  rt.vm.clear_module_graph();

  let err = rt.exec_compiled_script(script).unwrap_err();
  match err {
    VmError::Unimplemented(msg) => assert_eq!(msg, "dynamic import requires a module graph"),
    VmError::Throw(reason) | VmError::ThrowWithStack { value: reason, .. } => {
      let Value::Object(err_obj) = reason else {
        panic!("expected dynamic import error to throw an object, got {reason:?}");
      };
      let mut scope = rt.heap_mut().scope();
      scope.push_root(Value::Object(err_obj))?;
      let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
      let message = scope
        .heap()
        .object_get_own_data_property_value(err_obj, &message_key)?
        .expect("expected own message property");
      let Value::String(message_s) = message else {
        panic!("expected Error.message to be a string, got {message:?}");
      };
      let msg = scope.heap().get_string(message_s)?.to_utf8_lossy();
      assert!(
        msg.contains("dynamic import requires a module graph"),
        "expected message to mention missing module graph, got {msg:?}"
      );
    }
    other => panic!("expected unimplemented dynamic import error, got {other:?}"),
  }
  Ok(())
}

#[test]
fn compiled_import_call_returns_promise_like_value() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() { return import("x"); }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  struct ImportCallHooks {
    microtasks: vm_js::MicrotaskQueue,
  }

  impl VmHostHooks for ImportCallHooks {
    fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
      self.microtasks.enqueue_promise_job(job, realm);
    }

    fn host_load_imported_module(
      &mut self,
      vm: &mut Vm,
      scope: &mut vm_js::Scope<'_>,
      modules: &mut vm_js::ModuleGraph,
      referrer: vm_js::ModuleReferrer,
      module_request: vm_js::ModuleRequest,
      _host_defined: vm_js::HostDefined,
      payload: vm_js::ModuleLoadPayload,
    ) -> Result<(), VmError> {
      // Reject the dynamic import promise with a thrown value.
      //
      // `ContinueDynamicImport` treats thrown values as catchable rejections, while a plain
      // `VmError::Unimplemented` would propagate as an evaluator error.
      vm.finish_loading_imported_module(
        scope,
        modules,
        self,
        referrer,
        module_request,
        payload,
        Err(VmError::Throw(Value::Undefined)),
      )?;
      Ok(())
    }
  }

  let mut hooks = ImportCallHooks {
    microtasks: vm_js::MicrotaskQueue::new(),
  };
  let mut host = ();

  let realm_id = rt.realm().id();

  {
    let mut scope = rt.heap.scope();
    let name = scope.alloc_string("f")?;
    let f = scope.alloc_user_function(
      CompiledFunctionRef {
        script: script.clone(),
        body: f_body,
      },
      name,
      0,
    )?;

    // Dynamic import requires an active Realm. Ensure one is present even though we're calling the
    // function directly from Rust instead of via `JsRuntime::exec_*`.
    let exec_ctx = vm_js::ExecutionContext {
      realm: realm_id,
      script_or_module: None,
    };
    let mut vm_ctx = rt.vm.execution_context_guard(exec_ctx)?;

    let result = vm_ctx.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      Value::Object(f),
      Value::Undefined,
      &[],
    )?;

    let Value::Object(promise) = result else {
      panic!("expected import() result to be an object, got {result:?}");
    };
    assert!(
      scope.heap().is_promise_object(promise),
      "expected import() result to be a Promise object"
    );

    // Smoke-check that it is promise-like by asserting it has a callable `then` property.
    scope.push_root(Value::Object(promise))?;
    let then_key_s = scope.alloc_string("then")?;
    scope.push_root(Value::String(then_key_s))?;
    let then_key = vm_js::PropertyKey::from_string(then_key_s);

    let then = scope.get_with_host_and_hooks(
      &mut *vm_ctx,
      &mut host,
      &mut hooks,
      promise,
      then_key,
      Value::Object(promise),
    )?;
    assert!(
      scope.heap().is_callable(then)?,
      "expected Promise.then to be callable, got {then:?}"
    );
  }

  // Discard any jobs enqueued by Promise resolution/rejection so `Job` persistent roots are cleaned
  // up before the test ends.
  hooks.microtasks.teardown(&mut rt);
  Ok(())
}

#[test]
fn compiled_script_generator_function_falls_back_to_ecma() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function* g(){ yield 1; }
      let it = g();
      it.next().value
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_allows_defining_async_function_decls() -> Result<(), VmError> {
  let result = compile_and_call0(
    r#"
      function f() {
        async function g() { return 1; }
        return 2;
      }
    "#,
    "f",
  )?;
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn compiled_import_meta_in_module_returns_cached_object() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // `import.meta` is defined only in modules. Compile and execute a module through `ModuleGraph`
  // so the VM has an active module id and can consult per-graph `import.meta` caches.
  let realm_id = rt.realm().id();
  let global_object = rt.realm().global_object();
  let mut host = ();
  let mut hooks = vm_js::MicrotaskQueue::new();
  {
    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();

    let source = Arc::new(vm_js::SourceText::new_charged(
      heap,
      "test.js",
      r#"
        export const m1 = import.meta;
        export const m2 = import.meta;
      "#,
    )?);
    let record = vm_js::SourceTextModuleRecord::compile_source(heap, source)?;
    let module = modules.add_module(record)?;

    let mut scope = heap.scope();
    modules.evaluate_sync_with_scope(
      vm,
      &mut scope,
      global_object,
      realm_id,
      module,
      &mut host,
      &mut hooks,
    )?;

    // Read the two exported bindings from the module namespace.
    let ns = modules.get_module_namespace(module, vm, &mut scope)?;
    scope.push_root(Value::Object(ns))?;

    let key1_s = scope.alloc_string("m1")?;
    let key2_s = scope.alloc_string("m2")?;
    scope.push_roots(&[Value::String(key1_s), Value::String(key2_s)])?;

    let v1 = scope.get_with_host_and_hooks(
      vm,
      &mut host,
      &mut hooks,
      ns,
      vm_js::PropertyKey::from_string(key1_s),
      Value::Object(ns),
    )?;
    let v2 = scope.get_with_host_and_hooks(
      vm,
      &mut host,
      &mut hooks,
      ns,
      vm_js::PropertyKey::from_string(key2_s),
      Value::Object(ns),
    )?;

    let Value::Object(meta1) = v1 else {
      panic!("expected import.meta to be an object, got {v1:?}");
    };
    let Value::Object(meta2) = v2 else {
      panic!("expected import.meta to be an object, got {v2:?}");
    };

    // `import.meta` should be cached per module; repeated evaluations return the same object.
    assert_eq!(meta1, meta2);

    // Spec: `OrdinaryObjectCreate(null)` -> null-prototype object.
    assert_eq!(scope.object_get_prototype(meta1)?, None);
  }

  // Discard any jobs enqueued by Promise resolution/rejection so `Job` persistent roots are cleaned
  // up before the test ends.
  hooks.teardown(&mut rt);
  Ok(())
}

#[test]
fn compiled_module_throw_stack_has_statement_location() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let realm_id = rt.realm().id();
  let global_object = rt.realm().global_object();
  let mut hooks = vm_js::MicrotaskQueue::new();
  let mut host = ();

  // Execute a compiled module via `ModuleGraph` so the VM has a real module id in its active
  // `ScriptOrModule` context and module-scoped stack reporting can resolve statement locations.
  let err = {
    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();

    let source = Arc::new(vm_js::SourceText::new_charged(
      heap,
      "test.js",
      "function f() {\n  1;\n  throw \"x\";\n}\nf();\n",
    )?);
    let record = vm_js::SourceTextModuleRecord::compile_source(heap, source)?;
    let module = modules.add_module(record)?;

    let mut scope = heap.scope();
    modules
      .evaluate_sync_with_scope(
        vm,
        &mut scope,
        global_object,
        realm_id,
        module,
        &mut host,
        &mut hooks,
      )
      .unwrap_err()
  };

  let VmError::ThrowWithStack { stack, .. } = err else {
    return Err(err);
  };

  assert!(!stack.is_empty());
  assert_eq!(stack[0].source.as_ref(), "test.js");
  // `throw "x"` starts at line 3, column 3.
  assert_eq!((stack[0].line, stack[0].col), (3, 3));

  // Discard any jobs enqueued by Promise resolution/rejection so `Job` persistent roots are cleaned
  // up before the test ends.
  hooks.teardown(&mut rt);
  Ok(())
}

#[test]
fn compiled_new_constructs_compiled_user_function() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        function C(x) { this.x = x; }
        let o = new C(3);
        return o.x;
      }
      f();
    "#,
  )?;
  let value = rt.exec_compiled_script(script)?;
  assert_eq!(value, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_spread_call_args_work_for_call_and_new() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      function f() {
        function C(a, b) { this.v = a + b; }
        let args = Array(1, 2);
        let a = function(x, y) { return x + y; };
        return a(...args) + (new C(...args)).v;
      }
      f();
    "#,
  )?;
  let value = rt.exec_compiled_script(script)?;
  // a(...[1,2]) == 3 and (new C(...[1,2])).v == 3
  assert_eq!(value, Value::Number(6.0));
  Ok(())
}
