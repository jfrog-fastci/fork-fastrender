use crate::{
  Heap, HeapLimits, HostDefined, Job, ModuleGraph, ModuleLoadPayload, ModuleReferrer, ModuleRequest, PropertyDescriptor,
  PropertyKey, PropertyKind, RealmId, Scope, SourceText, SourceTextModuleRecord, Value, Vm, VmError, VmHostHooks,
  VmOptions,
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

struct FailingModuleLoadHooks {
  jobs: VecDeque<(Option<RealmId>, Job)>,
}

impl FailingModuleLoadHooks {
  fn new() -> Self {
    Self {
      jobs: VecDeque::new(),
    }
  }
}

impl VmHostHooks for FailingModuleLoadHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    self.jobs.push_back((realm, job));
  }

  fn host_load_imported_module(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _modules: &mut ModuleGraph,
    _referrer: ModuleReferrer,
    _module_request: ModuleRequest,
    _host_defined: HostDefined,
    _payload: ModuleLoadPayload,
  ) -> Result<(), VmError> {
    // Simulate a host hook returning a helper error while loading the static import graph.
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
fn exec_module_coerces_helper_errors_to_throw_with_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let type_error_proto = rt.realm().intrinsics().type_error_prototype();
  let mut host = ();
  let mut hooks = FailingModuleLoadHooks::new();
  let err = rt
    .exec_module_with_host_and_hooks(&mut host, &mut hooks, "m.js", "import \"./dep.js\";")
    .unwrap_err();

  let thrown_value = match err {
    VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected ThrowWithStack, got {other:?}"),
  };

  let Value::Object(thrown_obj) = thrown_value else {
    panic!("expected thrown value to be an object");
  };

  {
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

#[test]
fn evaluate_sync_coerces_unimplemented_to_throw_with_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global_object = rt.realm().global_object();
  let realm_id = rt.realm().id();
  let error_proto = rt.realm().intrinsics().error_prototype();

  let (err, mut hooks) = {
    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();

    let source = SourceText::new_charged_arc(heap, "m.js", "await 1;")?;
    let record = SourceTextModuleRecord::parse_source_with_vm(vm, source.clone())?;
    let module = modules.add_module_with_specifier("m.js", record)?;

    let mut host = ();
    let mut hooks = CapturingHooks::new();

    let mut scope = heap.scope();
    let err = modules
      .evaluate_sync_with_scope(vm, &mut scope, global_object, realm_id, module, &mut host, &mut hooks)
      .unwrap_err();

    (err, hooks)
  };

  // Ensure no queued jobs are dropped with leaked roots.
  while let Some((_realm, job)) = hooks.jobs.pop_front() {
    job.discard(&mut rt);
  }

  let thrown_value = match err {
    VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected ThrowWithStack, got {other:?}"),
  };

  let Value::Object(thrown_obj) = thrown_value else {
    panic!("expected thrown value to be an object");
  };

  let mut scope = rt.heap.scope();
  scope.push_root(thrown_value)?;
  assert_eq!(scope.heap().object_prototype(thrown_obj)?, Some(error_proto));
  Ok(())
}
