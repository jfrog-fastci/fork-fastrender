use vm_js::{
  Budget, Heap, HeapLimits, JsRuntime, PropertyKey, PropertyKind, TerminationReason, Value, Vm,
  VmError, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Use a larger heap so these tests fail due to budget exhaustion rather than OOM.
  let heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_termination_reason(err: VmError, expected: TerminationReason) {
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, expected),
    other => panic!("expected VmError::Termination({expected:?}), got {other:?}"),
  }
}

#[test]
fn fuel_stops_string_to_number_parsing() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.vm.set_budget(Budget {
    fuel: Some(10),
    deadline: None,
    check_time_every: 1,
  });

  // Allocate a very large string *outside* JS execution, then call `Number(hugeString)` in one go.
  // This ensures budget termination is driven by the internal String->Number parsing loop (not by
  // JS statement/expression ticks).
  let big = "0".repeat(1_000_000);

  let mut scope = rt.heap.scope();
  let arg = Value::String(scope.alloc_string(&big)?);

  let intr = rt.vm.intrinsics().ok_or(VmError::Unimplemented("intrinsics"))?;
  let mut host = ();
  let err = rt
    .vm
    .call(&mut host, &mut scope, Value::Object(intr.number_constructor()), Value::Undefined, &[arg])
    .unwrap_err();

  assert_termination_reason(err, TerminationReason::OutOfFuel);
  Ok(())
}

#[test]
fn fuel_stops_json_parse_on_large_input() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.vm.set_budget(Budget {
    fuel: Some(10),
    deadline: None,
    check_time_every: 1,
  });

  // Build a moderately-large JSON array. The key property is that JSON.parse performs substantial
  // internal scanning/copying work without executing user JS statements.
  let mut json = String::from("[");
  for i in 0..200_000usize {
    if i != 0 {
      json.push(',');
    }
    json.push('0');
  }
  json.push(']');

  let mut scope = rt.heap.scope();
  let json_arg = Value::String(scope.alloc_string(&json)?);

  let intr = rt.vm.intrinsics().ok_or(VmError::Unimplemented("intrinsics"))?;
  let json_obj = intr.json();
  let parse_key = PropertyKey::from_string(scope.alloc_string("parse")?);
  let Some(desc) = scope
    .heap()
    .get_property_with_tick(json_obj, &parse_key, || Ok(()))?
  else {
    panic!("missing JSON.parse");
  };
  let PropertyKind::Data { value: parse_func, .. } = desc.kind else {
    panic!("JSON.parse is not a data property");
  };

  let mut host = ();
  let err = rt
    .vm
    .call(&mut host, &mut scope, parse_func, Value::Undefined, &[json_arg])
    .unwrap_err();

  assert_termination_reason(err, TerminationReason::OutOfFuel);
  Ok(())
}

#[test]
fn fuel_stops_array_join_on_large_length() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.vm.set_budget(Budget {
    fuel: Some(10),
    deadline: None,
    check_time_every: 1,
  });

  let mut scope = rt.heap.scope();
  let intr = rt.vm.intrinsics().ok_or(VmError::Unimplemented("intrinsics"))?;

  // Create a sparse array with a very large length; `join` will still iterate `0..length`.
  let array = scope.alloc_array(1_000_000)?;
  scope
    .heap_mut()
    .object_set_prototype(array, Some(intr.array_prototype()))?;

  let join_key = PropertyKey::from_string(scope.alloc_string("join")?);
  let Some(desc) = scope
    .heap()
    .get_property_with_tick(intr.array_prototype(), &join_key, || Ok(()))?
  else {
    panic!("missing Array.prototype.join");
  };
  let PropertyKind::Data { value: join_func, .. } = desc.kind else {
    panic!("Array.prototype.join is not a data property");
  };

  let mut host = ();
  let err = rt
    .vm
    .call(&mut host, &mut scope, join_func, Value::Object(array), &[])
    .unwrap_err();

  assert_termination_reason(err, TerminationReason::OutOfFuel);
  Ok(())
}

