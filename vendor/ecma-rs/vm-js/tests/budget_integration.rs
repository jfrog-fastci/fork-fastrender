use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use vm_js::{
  Budget, GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind,
  Scope, TerminationReason, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

fn new_runtime_with_vm(vm: Vm) -> JsRuntime {
  // Use a larger heap for this test: we intentionally construct a large object and the output
  // array from `Object.keys`, and we want the failure mode to be budget termination rather than OOM.
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
fn fuel_stops_infinite_loop() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);

  rt.vm.set_budget(Budget {
    fuel: Some(10),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt.exec_script("for(;;){}").unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn interrupt_stops_execution() {
  let interrupt_flag = Arc::new(AtomicBool::new(false));
  let vm = Vm::new(VmOptions {
    interrupt_flag: Some(interrupt_flag.clone()),
    ..VmOptions::default()
  });
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget::unlimited(1));

  interrupt_flag.store(true, Ordering::Relaxed);

  let err = rt.exec_script("var x = 1; x = 2; x").unwrap_err();
  assert_termination_reason(err, TerminationReason::Interrupted);
}

#[test]
fn expression_evaluation_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);

  // The expression evaluator should tick at expression-granularity, so a small fuel budget should
  // be exhausted even if the script consists of a single expression statement.
  rt.vm.set_budget(Budget {
    fuel: Some(2),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt.exec_script("1 === 1").unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

fn native_noop(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Undefined)
}

#[test]
fn instantiation_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(10),
    deadline: None,
    check_time_every: 1,
  });

  // The statement list instantiation (hoisting + early checks) can traverse large trees even when
  // runtime evaluation is trivial. Ensure that work is budgeted.
  let mut src = String::from("if(false){");
  for i in 0..200 {
    src.push_str(&format!("var v{i};"));
  }
  src.push('}');

  let err = rt.exec_script(&src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn empty_script_ticks_at_entry() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(0),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt.exec_script("").unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn native_call_consumes_tick() {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let native_id = vm.register_native_call(native_noop).unwrap();
  let mut scope = heap.scope();
  let name = scope.alloc_string("f").unwrap();
  let callee = scope
    .alloc_native_function(native_id, None, name, 0)
    .unwrap();

  vm.set_budget(Budget {
    fuel: Some(0),
    deadline: None,
    check_time_every: 1,
  });

  let err = vm
    .call_without_host(&mut scope, Value::Object(callee), Value::Undefined, &[])
    .unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn builtins_function_apply_consumes_fuel_in_native_loop() {
  fn data_desc(value: Value) -> PropertyDescriptor {
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value,
        writable: true,
      },
    }
  }

  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap).unwrap();

  let (vm, realm, heap) = rt.vm_realm_and_heap_mut();
  let intr = realm.intrinsics();

  let mut scope = heap.scope();

  // Target function (doesn't matter what it does; we expect to run out of fuel while building the
  // argument list from the array-like object).
  let noop_id = vm.register_native_call(native_noop).unwrap();
  let noop_name = scope.alloc_string("noop").unwrap();
  let noop_fn = scope
    .alloc_native_function(noop_id, None, noop_name, 0)
    .unwrap();
  scope
    .heap_mut()
    .object_set_prototype(noop_fn, Some(intr.function_prototype()))
    .unwrap();
  scope.push_root(Value::Object(noop_fn)).unwrap();

  // Grab the intrinsic `Function.prototype.apply` function.
  let apply_key = PropertyKey::from_string(scope.alloc_string("apply").unwrap());
  let apply_func = match scope
    .heap()
    .get_property(intr.function_prototype(), &apply_key)
    .unwrap()
    .unwrap()
    .kind
  {
    PropertyKind::Data { value, .. } => value,
    PropertyKind::Accessor { .. } => panic!("Function.prototype.apply should be a data property"),
  };

  // Minimal array-like object: `{ length: 1025 }`.
  let arg_array = scope.alloc_object().unwrap();
  scope.push_root(Value::Object(arg_array)).unwrap();
  let length_key = PropertyKey::from_string(scope.alloc_string("length").unwrap());
  scope
    .define_property(arg_array, length_key, data_desc(Value::Number(1025.0)))
    .unwrap();

  vm.set_budget(Budget {
    fuel: Some(2),
    deadline: None,
    check_time_every: 1,
  });

  // Call `%Function.prototype.apply%` with `this = noop_fn`, `thisArg = undefined`,
  // `argArray = arg_array`.
  let err = vm
    .call_without_host(
      &mut scope,
      apply_func,
      Value::Object(noop_fn),
      &[Value::Undefined, Value::Object(arg_array)],
    )
    .unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn builtins_object_keys_consumes_fuel_in_native_loop() {
  fn enumerable_data_desc(value: Value) -> PropertyDescriptor {
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value,
        writable: true,
      },
    }
  }

  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap).unwrap();

  let (vm, realm, heap) = rt.vm_realm_and_heap_mut();
  let intr = realm.intrinsics();

  let mut scope = heap.scope();
  let obj = scope.alloc_object().unwrap();
  scope.push_root(Value::Object(obj)).unwrap();

  // One enumerable own property is enough to ensure `Object.keys` hits its internal loop.
  let key_s = scope.alloc_string("a").unwrap();
  let key = PropertyKey::from_string(key_s);
  scope
    .define_property(obj, key, enumerable_data_desc(Value::Number(1.0)))
    .unwrap();

  let keys_key = PropertyKey::from_string(scope.alloc_string("keys").unwrap());
  let object_ctor = intr.object_constructor();
  let keys_func = match scope
    .heap()
    .get_property(object_ctor, &keys_key)
    .unwrap()
    .unwrap()
    .kind
  {
    PropertyKind::Data { value, .. } => value,
    PropertyKind::Accessor { .. } => panic!("Object.keys should be a data property"),
  };

  // With only a single tick of fuel, the call-entry tick will succeed and the builtin will fail on
  // its first in-loop tick.
  vm.set_budget(Budget {
    fuel: Some(1),
    deadline: None,
    check_time_every: 1,
  });

  let err = vm
    .call_without_host(
      &mut scope,
      keys_func,
      Value::Object(object_ctor),
      &[Value::Object(obj)],
    )
    .unwrap_err();

  assert_termination_reason(err, TerminationReason::OutOfFuel);
}
