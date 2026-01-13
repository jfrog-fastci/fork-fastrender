use crate::{
  Heap, HeapLimits, Job, PropertyDescriptor, PropertyKey, PropertyKind, RealmId, Value, Vm, VmError,
  VmHostHooks, VmOptions,
};
use crate::exec::{eval_script_with_host_and_hooks, JsRuntime};
use std::collections::VecDeque;

#[test]
fn exec_script_coerces_instantiation_throw_to_throw_with_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Create a restricted global property so GlobalDeclarationInstantiation fails when the script
  // attempts to declare a same-named global function.
  let global = rt.realm().global_object();
  {
    let mut scope = rt.heap.scope();
    scope.push_root(Value::Object(global))?;
    let key_s = scope.alloc_string("x")?;
    let key = PropertyKey::from_string(key_s);
    scope.define_property(
      global,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Number(1.0),
          writable: false,
        },
      },
    )?;
  }

  let err = rt.exec_script("function x() {}").unwrap_err();
  let thrown_value = match err {
    VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected ThrowWithStack, got {other:?}"),
  };

  let Value::Object(thrown_obj) = thrown_value else {
    panic!("expected thrown value to be an object");
  };

  let type_error_proto = rt.realm().intrinsics().type_error_prototype();
  let mut scope = rt.heap.scope();
  scope.push_root(thrown_value)?;
  assert_eq!(scope.heap().object_prototype(thrown_obj)?, Some(type_error_proto));
  Ok(())
}

struct CapturingHooks {
  jobs: VecDeque<(Option<RealmId>, Job)>,
}

impl CapturingHooks {
  fn new() -> Self {
    Self {
      jobs: VecDeque::new(),
    }
  }
}

impl VmHostHooks for CapturingHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    self.jobs.push_back((realm, job));
  }

  fn host_call_job_callback(
    &mut self,
    _ctx: &mut dyn crate::VmJobContext,
    _callback: &crate::JobCallback,
    _this_argument: Value,
    _arguments: &[Value],
  ) -> Result<Value, VmError> {
    // Simulate a host hook returning a helper error from a Promise reaction job.
    Err(VmError::NotCallable)
  }
}

#[test]
fn job_run_coerces_helper_errors_to_throw_with_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let mut hooks = CapturingHooks::new();
  let _ = rt.exec_script_with_hooks(&mut hooks, "Promise.resolve(1).then(() => 2);")?;

  let (_realm, job) = hooks
    .jobs
    .pop_front()
    .expect("expected Promise reaction job to be enqueued");

  let err = job.run(&mut rt, &mut hooks).unwrap_err();
  let thrown_value = match err {
    VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected ThrowWithStack, got {other:?}"),
  };

  let Value::Object(thrown_obj) = thrown_value else {
    panic!("expected thrown value to be an object");
  };

  {
    let type_error_proto = rt.realm().intrinsics().type_error_prototype();
    let mut scope = rt.heap.scope();
    scope.push_root(thrown_value)?;
    assert_eq!(scope.heap().object_prototype(thrown_obj)?, Some(type_error_proto));
  }

  // Ensure no queued jobs are dropped with leaked roots.
  while let Some((_realm, job)) = hooks.jobs.pop_front() {
    job.discard(&mut rt);
  }

  Ok(())
}

#[test]
fn eval_script_coerces_instantiation_throw_to_throw_with_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Create a restricted global property so GlobalDeclarationInstantiation fails when the script
  // attempts to declare a same-named global function.
  let global = rt.realm().global_object();
  {
    let mut scope = rt.heap.scope();
    scope.push_root(Value::Object(global))?;
    let key_s = scope.alloc_string("x")?;
    let key = PropertyKey::from_string(key_s);
    scope.define_property(
      global,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Number(1.0),
          writable: false,
        },
      },
    )?;
  }

  let mut host = ();
  let mut hooks = CapturingHooks::new();
  let err = {
    let mut scope = rt.heap.scope();
    let source_string = scope.alloc_string("function x() {}")?;
    eval_script_with_host_and_hooks(&mut rt.vm, &mut scope, &mut host, &mut hooks, source_string)
      .unwrap_err()
  };

  let thrown_value = match err {
    VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected ThrowWithStack, got {other:?}"),
  };

  let Value::Object(thrown_obj) = thrown_value else {
    panic!("expected thrown value to be an object");
  };

  {
    let type_error_proto = rt.realm().intrinsics().type_error_prototype();
    let mut scope = rt.heap.scope();
    scope.push_root(thrown_value)?;
    assert_eq!(scope.heap().object_prototype(thrown_obj)?, Some(type_error_proto));
  }

  // Ensure no queued jobs are dropped with leaked roots.
  while let Some((_realm, job)) = hooks.jobs.pop_front() {
    job.discard(&mut rt);
  }

  Ok(())
}
