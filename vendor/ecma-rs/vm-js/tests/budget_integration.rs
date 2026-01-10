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

fn native_construct_noop(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _args: &[Value],
  _new_target: Value,
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
fn function_parameter_list_instantiation_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  // Function object creation computes `length` by scanning parameters until the first default/rest.
  // Large parameter lists should be budgeted so a single function declaration can't bypass fuel.
  let mut src = String::from("function f(");
  for i in 0..5000 {
    if i != 0 {
      src.push(',');
    }
    write!(src, "a{i}").unwrap();
  }
  src.push_str("){}");

  let err = rt.exec_script(&src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn function_use_strict_directive_scan_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  // Function object creation checks for a `"use strict"` directive by scanning the directive
  // prologue. The directive does not need to be the first statement, so a hostile script can place
  // it after many other string-literal directives and force an `O(N)` scan without evaluating any
  // nested statements/expressions.
  let mut src = String::from("function f(){");
  for _ in 0..5000 {
    src.push_str("\"x\";");
  }
  src.push_str("\"use strict\";}");

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
fn var_destructuring_pattern_instantiation_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  // A single `var` statement can contain a huge destructuring pattern. Put it behind `if(false)`
  // so runtime binding is skipped and the test isolates instantiation/hoisting work.
  let mut src = String::from("if(false){ var {");
  for i in 0..5000 {
    if i != 0 {
      src.push(',');
    }
    write!(src, "p{i}").unwrap();
  }
  src.push_str("} = o; }");

  let err = rt.exec_script(&src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn switch_case_list_instantiation_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  // `switch` statements can contain many case clauses, and instantiation passes may need to walk the
  // clause list even when the case bodies are empty. Ensure that large clause lists are budgeted.
  let mut src = String::from("if(false){ switch(0){");
  for i in 0..5000 {
    write!(src, "case {i}:").unwrap();
  }
  src.push_str("} }");

  let err = rt.exec_script(&src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn switch_case_list_evaluation_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(350),
    deadline: None,
    check_time_every: 1,
  });

  // Evaluation of a `switch` statement can iterate over large case clause lists even when the
  // clause bodies are empty (fallthrough after the first match). Ensure that this work is
  // budgeted, instead of being able to run after fuel is exhausted.
  let mut src = String::from("\"use strict\"; switch(0){");
  for i in 0..3000 {
    write!(src, "case {i}:").unwrap();
  }
  src.push('}');

  let err = rt.exec_script(&src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn block_function_decl_list_instantiation_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    // Intentionally high enough that hoisting/statement evaluation can make progress, but low
    // enough that block-entry instantiation of thousands of function declarations must be
    // budgeted.
    fuel: Some(3_500),
    deadline: None,
    check_time_every: 1,
  });

  // In strict mode, block-scoped function declarations are instantiated at block entry. Ensure
  // that large declaration lists cannot bypass fuel budgets by doing `O(N)` instantiation work
  // without ticking.
  let mut src = String::from("\"use strict\"; {");
  for i in 0..1000 {
    write!(src, "function f{i}(){{}}").unwrap();
  }
  src.push('}');

  let err = rt.exec_script(&src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn lexical_var_collision_check_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);

  // `instantiate_stmt_list` checks that lexical declarations do not collide with var-scoped names
  // (`var` + function declarations). The collision check loops all lexical binding names, so it
  // must be budgeted even when the only observable outcome is an early syntax error.
  //
  // Choose a budget that is sufficient for parsing + lexical name collection, but leaves no fuel
  // for the collision scan itself.
  let n = 100;
  rt.vm.set_budget(Budget {
    fuel: Some(n as u64 + 11),
    deadline: None,
    check_time_every: 1,
  });

  // The final `x` collides with the var declaration, but is placed at the end so the collision
  // scan must iterate the entire lexical binding list.
  let mut src = String::from("let ");
  for i in 0..n {
    if i != 0 {
      src.push(',');
    }
    write!(src, "a{i}").unwrap();
  }
  src.push_str(",x; var x;");

  let err = rt.exec_script(&src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn var_declarator_list_evaluation_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);

  // Instantiation does `O(N)` work for `var` declarations (collecting names and creating var
  // bindings). This test sets a budget high enough for instantiation to complete, but low enough
  // that runtime evaluation of a *single* large declaration statement must also be budgeted (i.e.
  // we must tick per declarator even when there are no initializer expressions).
  rt.vm.set_budget(Budget {
    fuel: Some(3800),
    deadline: None,
    check_time_every: 1,
  });

  let mut src = String::from("var ");
  for i in 0..1000 {
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
fn let_declarator_list_evaluation_consumes_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);

  // `for (let ...; ...)` declarations are evaluated at runtime, and (in this evaluator) are not
  // instantiated as part of the surrounding statement list hoisting. This provides a narrow
  // regression test for the runtime declarator loop budget.
  rt.vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  let mut src = String::from("for (let ");
  for i in 0..2000 {
    if i != 0 {
      src.push(',');
    }
    src.push_str(&format!("v{i}"));
  }
  src.push_str("; false;) {}");

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
fn parsing_respects_fuel_budget() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);
  rt.vm.set_budget(Budget {
    fuel: Some(0),
    deadline: None,
    check_time_every: 1,
  });

  // Use a large script that would otherwise take noticeable time to parse before failing with a
  // syntax error at the end.
  let mut src = String::new();
  for _ in 0..10_000 {
    src.push_str("1;");
  }
  src.push('}');

  let err = rt.exec_script(&src).unwrap_err();
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
fn large_args_call_consumes_fuel_before_dispatch() {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));

  let native_id = vm.register_native_call(native_noop).unwrap();
  let mut scope = heap.scope();
  let name = scope.alloc_string("f").unwrap();
  let callee = scope
    .alloc_native_function(native_id, None, name, 0)
    .unwrap();

  vm.set_budget(Budget {
    fuel: Some(1),
    deadline: None,
    check_time_every: 1,
  });

  let args = vec![Value::Undefined; 16 * 1024];
  let err = vm
    .call_without_host(&mut scope, Value::Object(callee), Value::Undefined, &args)
    .unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn large_args_construct_consumes_fuel_before_dispatch() {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));

  let call_id = vm.register_native_call(native_noop).unwrap();
  let construct_id = vm.register_native_construct(native_construct_noop).unwrap();
  let mut scope = heap.scope();
  let name = scope.alloc_string("f").unwrap();
  let callee = scope
    .alloc_native_function(call_id, Some(construct_id), name, 0)
    .unwrap();

  vm.set_budget(Budget {
    fuel: Some(1),
    deadline: None,
    check_time_every: 1,
  });

  let args = vec![Value::Undefined; 16 * 1024];
  let err = vm
    .construct_without_host(&mut scope, Value::Object(callee), &args, Value::Object(callee))
    .unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn large_bound_args_call_consumes_fuel_before_dispatch() {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));

  let call_id = vm.register_native_call(native_noop).unwrap();
  let mut scope = heap.scope();

  let target_name = scope.alloc_string("target").unwrap();
  let target = scope
    .alloc_native_function(call_id, None, target_name, 0)
    .unwrap();

  let bound_name = scope.alloc_string("bound").unwrap();
  let bound_args = vec![Value::Undefined; 16 * 1024];
  let bound = scope
    .alloc_bound_function(target, Value::Undefined, &bound_args, bound_name, 0)
    .unwrap();

  vm.set_budget(Budget {
    fuel: Some(1),
    deadline: None,
    check_time_every: 1,
  });

  let err = vm
    .call_without_host(&mut scope, Value::Object(bound), Value::Undefined, &[])
    .unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn large_bound_args_construct_consumes_fuel_before_dispatch() {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));

  let call_id = vm.register_native_call(native_noop).unwrap();
  let construct_id = vm.register_native_construct(native_construct_noop).unwrap();
  let mut scope = heap.scope();

  let target_name = scope.alloc_string("target").unwrap();
  let target = scope
    .alloc_native_function(call_id, Some(construct_id), target_name, 0)
    .unwrap();

  let bound_name = scope.alloc_string("bound").unwrap();
  let bound_args = vec![Value::Undefined; 16 * 1024];
  let bound = scope
    .alloc_bound_function(target, Value::Undefined, &bound_args, bound_name, 0)
    .unwrap();

  vm.set_budget(Budget {
    fuel: Some(1),
    deadline: None,
    check_time_every: 1,
  });

  let err = vm
    .construct_without_host(&mut scope, Value::Object(bound), &[], Value::Object(bound))
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
fn array_destructuring_assignment_elements_consume_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);

  // Destructuring *assignment* does not participate in declaration instantiation/hoisting, so the
  // only scalable work here is runtime destructuring. Ensure the per-element binding work is
  // budgeted even when the RHS expression is trivial (`src`).
  rt.vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  let mut src = String::from("var src = { length: 2000 }; [");
  for i in 0..2000 {
    if i != 0 {
      src.push(',');
    }
    write!(src, "a{i}").unwrap();
  }
  src.push_str("] = src;");

  let err = rt.exec_script(&src).unwrap_err();
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
