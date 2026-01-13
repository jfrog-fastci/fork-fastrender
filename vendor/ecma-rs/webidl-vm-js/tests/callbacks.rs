use vm_js::{
  Heap, HeapLimits, Job, PropertyKey, Scope, Value, Vm, VmError, VmHostHooks, VmOptions,
};
use webidl_vm_js::{
  invoke_callback_function, invoke_callback_interface, to_callback_function, to_callback_interface,
};

#[derive(Default)]
struct JobQueueHooks {
  jobs: Vec<(Job, Option<vm_js::RealmId>)>,
}

impl VmHostHooks for JobQueueHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<vm_js::RealmId>) {
    self.jobs.push((job, realm));
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn get_global(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  global: vm_js::GcObject,
  name: &str,
) -> Result<Value, VmError> {
  scope.push_root(Value::Object(global))?;
  let key = alloc_key(scope, name)?;
  vm.get(scope, global, key)
}

#[test]
fn callback_function_conversion_rejects_non_callable_and_legacy_coerces_primitives_to_null() {
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let err = to_callback_function(&heap, Value::Number(1.0), false).unwrap_err();
  assert!(matches!(err, VmError::TypeError(_)));

  let got = to_callback_function(&heap, Value::Number(1.0), true).unwrap();
  assert_eq!(got, Value::Null);

  assert!(to_callback_interface(&heap, Value::Number(1.0)).is_err());
}

#[test]
fn invoke_callback_function_binds_this_and_enqueues_microtasks_via_hooks() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = vm_js::JsRuntime::new(vm, heap)?;

  // A strict callback that:
  // - records `this` + args,
  // - schedules a Promise job,
  // - and returns a sentinel.
  rt.exec_script(
    r#"
globalThis.expectedThis = {};
globalThis.seenThis = undefined;
globalThis.seenA = undefined;
globalThis.seenB = undefined;
globalThis.microtaskRan = 0;
globalThis.cb = function(a, b) {
  "use strict";
  globalThis.seenThis = this;
  globalThis.seenA = a;
  globalThis.seenB = b;
  Promise.resolve().then(() => { globalThis.microtaskRan = 1; });
  return 7;
};
"#,
  )?;

  let mut hooks = JobQueueHooks::default();

  // Invoke the callback via the helper.
  {
    let (vm, realm, heap) = rt.vm_realm_and_heap_mut();
    let global = realm.global_object();
    let mut scope = heap.scope();

    let cb = get_global(vm, &mut scope, global, "cb")?;
    let expected_this = get_global(vm, &mut scope, global, "expectedThis")?;

    let out = invoke_callback_function(
      vm,
      &mut scope,
      &mut hooks,
      cb,
      expected_this,
      &[Value::Number(1.0), Value::Number(2.0)],
    )?;
    assert_eq!(out, Value::Number(7.0));

    let seen_this = get_global(vm, &mut scope, global, "seenThis")?;
    assert_eq!(seen_this, expected_this);
    let seen_a = get_global(vm, &mut scope, global, "seenA")?;
    assert_eq!(seen_a, Value::Number(1.0));
    let seen_b = get_global(vm, &mut scope, global, "seenB")?;
    assert_eq!(seen_b, Value::Number(2.0));
  }

  assert!(
    !hooks.jobs.is_empty(),
    "expected Promise.then to enqueue at least one job via host hooks"
  );

  // Drive the queued jobs using the runtime as the job context.
  while let Some((job, _realm)) = hooks.jobs.pop() {
    job.run(&mut rt, &mut hooks)?;
  }

  {
    let (vm, realm, heap) = rt.vm_realm_and_heap_mut();
    let global = realm.global_object();
    let mut scope = heap.scope();
    let ran = get_global(vm, &mut scope, global, "microtaskRan")?;
    assert_eq!(ran, Value::Number(1.0));
  }

  Ok(())
}

#[test]
fn invoke_callback_interface_calls_handle_event_with_object_this() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = vm_js::JsRuntime::new(vm, heap)?;

  rt.exec_script(
    r#"
globalThis.seenThis = undefined;
globalThis.seenArg = undefined;
globalThis.listener = {
  handleEvent(e) {
    "use strict";
    globalThis.seenThis = this;
    globalThis.seenArg = e;
    return 9;
  }
};
"#,
  )?;

  let mut hooks = JobQueueHooks::default();

  {
    let (vm, realm, heap) = rt.vm_realm_and_heap_mut();
    let global = realm.global_object();
    let mut scope = heap.scope();
    let listener = get_global(vm, &mut scope, global, "listener")?;

    let out = invoke_callback_interface(
      vm,
      &mut scope,
      &mut hooks,
      listener,
      Value::Undefined,
      &[Value::Number(123.0)],
    )?;
    assert_eq!(out, Value::Number(9.0));

    let seen_this = get_global(vm, &mut scope, global, "seenThis")?;
    assert_eq!(seen_this, listener);
    let seen_arg = get_global(vm, &mut scope, global, "seenArg")?;
    assert_eq!(seen_arg, Value::Number(123.0));
  }

  Ok(())
}

#[test]
fn invoke_callback_interface_calls_accept_node_with_object_this() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = vm_js::JsRuntime::new(vm, heap)?;

  rt.exec_script(
    r#"
globalThis.seenThis = undefined;
globalThis.seenArg = undefined;
globalThis.filter = {
  acceptNode(n) {
    "use strict";
    globalThis.seenThis = this;
    globalThis.seenArg = n;
    return 11;
  }
};
"#,
  )?;

  let mut hooks = JobQueueHooks::default();

  {
    let (vm, realm, heap) = rt.vm_realm_and_heap_mut();
    let global = realm.global_object();
    let mut scope = heap.scope();
    let filter = get_global(vm, &mut scope, global, "filter")?;

    let out = invoke_callback_interface(
      vm,
      &mut scope,
      &mut hooks,
      filter,
      Value::Undefined,
      &[Value::Number(123.0)],
    )?;
    assert_eq!(out, Value::Number(11.0));

    let seen_this = get_global(vm, &mut scope, global, "seenThis")?;
    assert_eq!(seen_this, filter);
    let seen_arg = get_global(vm, &mut scope, global, "seenArg")?;
    assert_eq!(seen_arg, Value::Number(123.0));
  }

  Ok(())
}
