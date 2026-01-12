use std::sync::Arc;
use vm_js::{
  Budget, CompiledFunctionRef, CompiledScript, Heap, HeapLimits, TerminationReason, Value, Vm,
  VmError, VmOptions,
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
