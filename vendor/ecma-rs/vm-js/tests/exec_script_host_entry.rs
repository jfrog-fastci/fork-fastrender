use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use vm_js::{
  GcObject, Heap, HeapLimits, Job, JobKind, JsRuntime, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks, VmOptions,
};

fn current_realm_is_set(
  vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Bool(vm.current_realm().is_some()))
}

fn call_callback_via_vm_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let func = args.first().copied().unwrap_or(Value::Undefined);
  vm.call(host, scope, func, Value::Undefined, &[])?;
  Ok(Value::Undefined)
}

fn enqueue_vm_microtask_job(
  vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let counter = vm
    .user_data::<Arc<AtomicUsize>>()
    .cloned()
    .ok_or(VmError::Unimplemented("missing Arc<AtomicUsize> user_data"))?;

  // Intentionally enqueue onto the VM-owned microtask queue (instead of the supplied host hooks).
  // Script execution should preserve this even though it temporarily moves the queue out of the VM
  // to satisfy Rust borrow constraints.
  let job = Job::new(JobKind::Promise, move |_ctx, _host| {
    counter.fetch_add(1, Ordering::SeqCst);
    Ok(())
  })?;
  vm.microtask_queue_mut().enqueue_promise_job(job, None);

  Ok(Value::Undefined)
}

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn exec_script_sets_current_realm_during_evaluation() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("currentRealmIsSet", current_realm_is_set, 0)?;

  let value = rt.exec_script("currentRealmIsSet()")?;
  assert_eq!(value, Value::Bool(true));

  // `exec_script` should restore the execution context stack to its prior state.
  assert_eq!(rt.vm.current_realm(), None);

  Ok(())
}

#[test]
fn exec_script_sets_current_realm_during_evaluation_even_on_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("currentRealmIsSet", current_realm_is_set, 0)?;

  let err = rt
    .exec_script(
      r#"
        var ok = currentRealmIsSet();
        throw 1;
      "#,
    )
    .unwrap_err();
  assert!(
    matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }),
    "expected a JS throw, got {err:?}"
  );

  // `exec_script` should restore the execution context stack to its prior state.
  assert_eq!(rt.vm.current_realm(), None);

  // The value computed before the throw should have observed a valid current realm.
  let value = rt.exec_script("ok")?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn exec_script_sets_active_host_hooks_override_to_vm_microtask_queue() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("callCallback", call_callback_via_vm_call, 1)?;

  // Queue a Promise job from inside a nested `Vm::call` invoked by a native function.
  // Without an active host hooks override, the nested call would enqueue onto the VM-owned queue
  // and then get lost when the outer script execution restores its moved-out queue.
  let value = rt.exec_script(
    r#"
      var ran = false;
      function schedule() {
        Promise.resolve().then(() => { ran = true; });
      }
      callCallback(schedule);
      ran;
    "#,
  )?;
  assert_eq!(value, Value::Bool(false));

  // Run any queued Promise jobs.
  let mut queue = std::mem::take(rt.vm.microtask_queue_mut());
  let errors = queue.perform_microtask_checkpoint(&mut rt);
  *rt.vm.microtask_queue_mut() = queue;
  assert!(errors.is_empty());

  // The Promise job should have been preserved and run.
  let value = rt.exec_script("ran")?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn exec_script_restores_microtask_queue_even_on_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("callCallback", call_callback_via_vm_call, 1)?;

  let err = rt
    .exec_script(
      r#"
        var ran = false;
        function schedule() {
          Promise.resolve().then(() => { ran = true; });
        }
        callCallback(schedule);
        throw 1;
      "#,
    )
    .unwrap_err();
  assert!(
    matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }),
    "expected a JS throw, got {err:?}"
  );

  // Run any queued Promise jobs.
  let mut queue = std::mem::take(rt.vm.microtask_queue_mut());
  let errors = queue.perform_microtask_checkpoint(&mut rt);
  *rt.vm.microtask_queue_mut() = queue;
  assert!(errors.is_empty());

  // The Promise job should have been preserved and run even though the script threw.
  let value = rt.exec_script("ran")?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn exec_script_preserves_jobs_enqueued_directly_on_vm_microtask_queue() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let counter = Arc::new(AtomicUsize::new(0));
  rt.vm.set_user_data(counter.clone());
  rt.register_global_native_function("enqueueVmJob", enqueue_vm_microtask_job, 0)?;

  rt.exec_script("enqueueVmJob();")?;

  // The job should not have run yet.
  assert_eq!(counter.load(Ordering::SeqCst), 0);

  let mut queue = std::mem::take(rt.vm.microtask_queue_mut());
  let errors = queue.perform_microtask_checkpoint(&mut rt);
  *rt.vm.microtask_queue_mut() = queue;
  assert!(errors.is_empty());

  assert_eq!(counter.load(Ordering::SeqCst), 1);
  Ok(())
}
