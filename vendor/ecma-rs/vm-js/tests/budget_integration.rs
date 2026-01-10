use std::fmt::Write;
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
fn var_declarator_list_instantiation_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  // A single `var` statement can contain a huge number of declarators. Ensure instantiation work is
  // budgeted at declarator/pattern granularity instead of only per statement.
  let mut src = String::from("var ");
  for i in 0..5000 {
    if i != 0 {
      src.push(',');
    }
    src.push_str(&format!("v{i}"));
  }
  src.push(';');

  let err = rt.exec_script(&src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn let_declarator_list_instantiation_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  // Block-scoped `let` declarations are instantiated at block entry; large declarator lists should
  // also be budgeted.
  let mut src = String::from("{ let ");
  for i in 0..5000 {
    if i != 0 {
      src.push(',');
    }
    src.push_str(&format!("v{i}"));
  }
  src.push_str("; }");

  let err = rt.exec_script(&src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn destructuring_var_decl_binding_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);

  // Create the RHS object with an unlimited budget so this test isolates the cost of the
  // destructuring pattern traversal/binding in the second script.
  rt.vm.set_budget(Budget::unlimited(1));
  rt.exec_script("var o = {};").unwrap();

  // A large destructuring pattern can do significant work even when:
  // - the initializer expression is trivial, and
  // - there are no computed keys / default values.
  //
  // The fuel budget is chosen so that instantiation succeeds, but runtime binding must also be
  // budgeted (i.e. it cannot be "free" work).
  let mut src = String::from("var {");
  for i in 0..2000 {
    if i != 0 {
      src.push(',');
    }
    write!(src, "p{i}").unwrap();
  }
  src.push_str("} = o;");

  rt.vm.set_budget(Budget {
    fuel: Some(7000),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt.exec_script(&src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn object_destructuring_rest_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  install_global_object_with_enumerable_props(&mut rt, "obj", 4096);

  // Object rest patterns can copy an arbitrary number of enumerable keys even when the pattern is
  // tiny (`{...r}`). Ensure the copy loop is budgeted.
  rt.vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt.exec_script("var {...r} = obj;").unwrap_err();
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

fn install_global_object_with_enumerable_props(
  rt: &mut JsRuntime,
  global_name: &str,
  prop_count: u32,
) -> GcObject {
  let global = rt.realm().global_object();
  let mut scope = rt.heap_mut().scope();

  let obj = scope.alloc_object().unwrap();
  scope.push_root(Value::Object(obj)).unwrap();

  for i in 0..prop_count {
    let mut prop_scope = scope.reborrow();
    let key_s = prop_scope.alloc_string(&format!("k{i}")).unwrap();
    prop_scope.push_root(Value::String(key_s)).unwrap();
    let key = PropertyKey::from_string(key_s);
    assert!(prop_scope
      .create_data_property(obj, key, Value::Number(i as f64))
      .unwrap());
  }

  let mut global_scope = scope.reborrow();
  let global_key_s = global_scope.alloc_string(global_name).unwrap();
  global_scope.push_root(Value::String(global_key_s)).unwrap();
  let global_key = PropertyKey::from_string(global_key_s);
  assert!(global_scope
    .create_data_property(global, global_key, Value::Object(obj))
    .unwrap());

  obj
}

#[test]
fn for_in_key_collection_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  install_global_object_with_enumerable_props(&mut rt, "obj", 4096);

  // The `for..in` evaluator snapshots enumerable keys *before* iteration begins. Previously this key
  // collection phase had no internal ticks, so a large object could bypass fuel budgets even when
  // the loop body exits immediately.
  rt.vm.set_budget(Budget {
    fuel: Some(20),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt.exec_script("for (var k in obj) { break; }").unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn array_destructuring_rest_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);

  let global = rt.realm().global_object();
  {
    let mut scope = rt.heap_mut().scope();
    let src = scope.alloc_object().unwrap();
    scope.push_root(Value::Object(src)).unwrap();

    {
      let mut len_scope = scope.reborrow();
      let len_key_s = len_scope.alloc_string("length").unwrap();
      len_scope.push_root(Value::String(len_key_s)).unwrap();
      let len_key = PropertyKey::from_string(len_key_s);
      assert!(len_scope
        .create_data_property(src, len_key, Value::Number(4096.0))
        .unwrap());
    }

    {
      let mut global_scope = scope.reborrow();
      let src_key_s = global_scope.alloc_string("src").unwrap();
      global_scope.push_root(Value::String(src_key_s)).unwrap();
      let src_key = PropertyKey::from_string(src_key_s);
      assert!(global_scope
        .create_data_property(global, src_key, Value::Object(src))
        .unwrap());
    }
  }

  // `bind_array_pattern`'s rest loop can perform large amounts of work within a single expression.
  // Ensure that it is budgeted.
  rt.vm.set_budget(Budget {
    fuel: Some(12),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt.exec_script("var [...r] = src;").unwrap_err();
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
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
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

#[test]
fn spread_call_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  // Spread expansion must tick per iterated element, even for Array fast-path iterators.
  let err = rt
    .exec_script("let a=[]; a.length=5000; function f(){}; f(...a);")
    .unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn spread_new_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt
    .exec_script("function C(){}; let a=[]; a.length=5000; new C(...a);")
    .unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn holey_array_literal_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  // Large holey array literals previously performed `O(N)` work with only the enclosing expression
  // tick, allowing budget checks to be bypassed.
  let src = format!("[{}];", ",".repeat(100_000));
  let err = rt.exec_script(&src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn object_literal_methods_or_shorthand_consume_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  // Object literal methods don't evaluate nested expressions, so a large number of members could
  // previously do `O(N)` work while consuming only the enclosing expression tick.
  let mut src = String::from("({");
  for i in 0..20_000 {
    write!(src, "m{i}(){{}},").unwrap();
  }
  src.push_str("});");

  let err = rt.exec_script(&src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn function_prologue_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);

  rt.vm.set_budget(Budget::unlimited(1));
  let f = rt.exec_script("function f(){} f").unwrap();
  let Value::Object(f) = f else {
    panic!("expected function object, got {f:?}");
  };

  let args: Vec<Value> = (0..512).map(|i| Value::Number(i as f64)).collect();

  rt.vm.set_budget(Budget {
    fuel: Some(10),
    deadline: None,
    check_time_every: 1,
  });

  let mut scope = rt.heap.scope();
  let err = rt
    .vm
    .call_without_host(&mut scope, Value::Object(f), Value::Undefined, &args)
    .unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}
