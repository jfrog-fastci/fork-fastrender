use std::collections::{HashMap, VecDeque};

use vm_js::{
  load_requested_modules, perform_promise_then, Heap, HeapLimits, HostDefined, ImportAttribute, Job,
  JobCallback, JsString, ModuleCompletion, ModuleGraph, ModuleLoadPayload, ModuleReferrer,
  ModuleRequest, ModuleStatus, PromiseState, PropertyKey, PropertyKind, Realm, RootId, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext, VmOptions,
};

#[derive(Clone)]
enum PlannedLoad {
  Sync(ModuleCompletion),
  Async(ModuleCompletion),
}

struct PendingLoad {
  referrer: ModuleReferrer,
  request: ModuleRequest,
  payload: ModuleLoadPayload,
  result: ModuleCompletion,
}

#[derive(Default)]
struct FakeHost {
  plan: HashMap<JsString, PlannedLoad>,
  pending: Vec<PendingLoad>,
  jobs: VecDeque<Job>,
  callback_calls: Vec<vm_js::GcObject>,
}

impl FakeHost {
  fn plan_sync(&mut self, specifier: &str, result: ModuleCompletion) {
    self
      .plan
      .insert(JsString::from_str(specifier).unwrap(), PlannedLoad::Sync(result));
  }

  fn plan_async(&mut self, specifier: &str, result: ModuleCompletion) {
    self
      .plan
      .insert(JsString::from_str(specifier).unwrap(), PlannedLoad::Async(result));
  }

  fn complete_pending(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    index: usize,
  ) {
    let pending = self.pending.remove(index);
    vm.finish_loading_imported_module(
      scope,
      modules,
      self,
      pending.referrer,
      pending.request,
      pending.payload,
      pending.result,
    )
    .unwrap();
  }
}

impl VmHostHooks for FakeHost {
  fn host_enqueue_promise_job(&mut self, job: Job, _realm: Option<vm_js::RealmId>) {
    self.jobs.push_back(job);
  }

  fn host_call_job_callback(
    &mut self,
    _ctx: &mut dyn VmJobContext,
    callback: &JobCallback,
    _this_argument: Value,
    _arguments: &[Value],
  ) -> Result<Value, VmError> {
    self.callback_calls.push(callback.callback_object());
    Ok(Value::Undefined)
  }

  fn host_load_imported_module(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    _host_defined: HostDefined,
    payload: ModuleLoadPayload,
  ) -> Result<(), VmError> {
    let action = self
      .plan
      .get(&module_request.specifier)
      .unwrap_or_else(|| panic!("unexpected module request {:?}", module_request.specifier))
      .clone();

    match action {
      PlannedLoad::Sync(result) => {
        vm.finish_loading_imported_module(
          scope,
          modules,
          self,
          referrer,
          module_request,
          payload,
          result,
        )
      }
      PlannedLoad::Async(result) => {
        self.pending.push(PendingLoad {
          referrer,
          request: module_request,
          payload,
          result,
        });
        Ok(())
      }
    }
  }
}

fn req(specifier: &str) -> ModuleRequest {
  ModuleRequest::new(JsString::from_str(specifier).unwrap(), vec![])
}

fn req_with_attr(specifier: &str, key: &str, value: &str) -> ModuleRequest {
  ModuleRequest::new(
    JsString::from_str(specifier).unwrap(),
    vec![ImportAttribute::new(key, value)],
  )
}

fn record(requested: Vec<ModuleRequest>) -> SourceTextModuleRecord {
  let mut record = SourceTextModuleRecord::default();
  record.requested_modules = requested;
  record
}

fn new_vm_and_heap() -> (Vm, Heap, Realm) {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let realm = Realm::new(&mut vm, &mut heap).unwrap();
  (vm, heap, realm)
}

fn noop_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Undefined)
}

struct JobCtx<'a> {
  vm: &'a mut Vm,
  heap: &'a mut Heap,
}

impl VmJobContext for JobCtx<'_> {
  fn call(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let mut scope = self.heap.scope();
    self.vm.call_with_host(&mut scope, host, callee, this, args)
  }

  fn construct(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let mut scope = self.heap.scope();
    self
      .vm
      .construct_with_host(&mut scope, host, callee, args, new_target)
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id)
  }
}

#[test]
fn simple_graph_resolves() {
  let mut modules = ModuleGraph::new();
  let b = modules.add_module(record(Vec::new())).expect("add module");
  let c = modules.add_module(record(Vec::new())).expect("add module");
  let a = modules
    .add_module(record(vec![req("B"), req("C")]))
    .expect("add module");

  let (mut vm, mut heap, mut realm) = new_vm_and_heap();

  let mut host = FakeHost::default();
  host.plan_async("B", Ok(b));
  host.plan_async("C", Ok(c));

  {
    let mut scope = heap.scope();
    let promise =
      load_requested_modules(&mut vm, &mut scope, &mut modules, &mut host, a, HostDefined::default())
        .unwrap();
    scope.push_root(promise).unwrap();

    let Value::Object(promise_obj) = promise else {
      panic!("LoadRequestedModules should return a Promise object");
    };

    assert_eq!(
      scope.heap().promise_state(promise_obj).unwrap(),
      PromiseState::Pending
    );
    assert_eq!(host.pending.len(), 2);

    // Complete out-of-order.
    host.complete_pending(&mut vm, &mut scope, &mut modules, 1);
    assert_eq!(
      scope.heap().promise_state(promise_obj).unwrap(),
      PromiseState::Pending
    );
    host.complete_pending(&mut vm, &mut scope, &mut modules, 0);
    assert_eq!(
      scope.heap().promise_state(promise_obj).unwrap(),
      PromiseState::Fulfilled
    );

    assert_eq!(modules.module(a).status, ModuleStatus::Unlinked);
    assert_eq!(modules.module(b).status, ModuleStatus::Unlinked);
    assert_eq!(modules.module(c).status, ModuleStatus::Unlinked);
  }

  realm.teardown(&mut heap);
}

#[test]
fn graph_loading_routes_promise_jobs_through_host_hooks() {
  let mut modules = ModuleGraph::new();
  let b = modules.add_module(record(Vec::new())).expect("add module");
  let a = modules.add_module(record(vec![req("B")])).expect("add module");

  let (mut vm, mut heap, mut realm) = new_vm_and_heap();

  let mut host = FakeHost::default();
  host.plan_async("B", Ok(b));

  let (then1, then2) = {
    let mut scope = heap.scope();
    let promise =
      load_requested_modules(&mut vm, &mut scope, &mut modules, &mut host, a, HostDefined::default())
        .unwrap();
    scope.push_root(promise).unwrap();

    // Attach multiple promise reactions so fulfillment enqueues multiple Promise jobs.
    let call_id = vm.register_native_call(noop_native).unwrap();
    let name1 = scope.alloc_string("then1").unwrap();
    let then1 = scope.alloc_native_function(call_id, None, name1, 1).unwrap();
    let name2 = scope.alloc_string("then2").unwrap();
    let then2 = scope.alloc_native_function(call_id, None, name2, 1).unwrap();

    // Root callbacks while attaching them (promise operations allocate and may GC).
    scope.push_root(Value::Object(then1)).unwrap();
    scope.push_root(Value::Object(then2)).unwrap();

    perform_promise_then(
      &mut vm,
      &mut scope,
      &mut host,
      promise,
      Some(Value::Object(then1)),
      None,
    )
    .unwrap();
    perform_promise_then(
      &mut vm,
      &mut scope,
      &mut host,
      promise,
      Some(Value::Object(then2)),
      None,
    )
    .unwrap();

    assert!(vm.microtask_queue().is_empty());

    // Completing the outstanding module load resolves the graph-loading promise, enqueueing the
    // reaction jobs via `host_enqueue_promise_job`.
    assert_eq!(host.pending.len(), 1);
    host.complete_pending(&mut vm, &mut scope, &mut modules, 0);

    assert!(vm.microtask_queue().is_empty(), "jobs should not be routed into Vm::microtask_queue");
    assert_eq!(host.jobs.len(), 2, "expected one Promise job per .then handler");

    (then1, then2)
  };

  // Drain the host-owned Promise job queue and record callback invocation order.
  let mut ctx = JobCtx { vm: &mut vm, heap: &mut heap };
  while let Some(job) = host.jobs.pop_front() {
    job.run(&mut ctx, &mut host).unwrap();
  }

  assert_eq!(host.callback_calls, vec![then1, then2]);
  assert!(vm.microtask_queue().is_empty());

  realm.teardown(&mut heap);
}

#[test]
fn cycle_does_not_infinite_loop() {
  let mut modules = ModuleGraph::new();
  let a = modules.add_module(record(vec![req("B")])).expect("add module");
  let b = modules.add_module(record(vec![req("A")])).expect("add module");

  let (mut vm, mut heap, mut realm) = new_vm_and_heap();

  let mut host = FakeHost::default();
  host.plan_sync("A", Ok(a));
  host.plan_sync("B", Ok(b));

  {
    let mut scope = heap.scope();
    let promise =
      load_requested_modules(&mut vm, &mut scope, &mut modules, &mut host, a, HostDefined::default())
        .unwrap();
    scope.push_root(promise).unwrap();

    let Value::Object(promise_obj) = promise else {
      panic!("LoadRequestedModules should return a Promise object");
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj).unwrap(),
      PromiseState::Fulfilled
    );
    assert!(host.pending.is_empty());

    assert_eq!(modules.module(a).status, ModuleStatus::Unlinked);
    assert_eq!(modules.module(b).status, ModuleStatus::Unlinked);
  }

  realm.teardown(&mut heap);
}

#[test]
fn load_failure_rejects_and_freezes_state() {
  let mut modules = ModuleGraph::new();
  let b = modules.add_module(record(Vec::new())).expect("add module");
  let c = modules.add_module(record(Vec::new())).expect("add module");
  let a = modules
    .add_module(record(vec![req("B"), req("C")]))
    .expect("add module");

  let (mut vm, mut heap, mut realm) = new_vm_and_heap();

  let mut host = FakeHost::default();
  host.plan_async("B", Ok(b));
  host.plan_sync("C", Err(VmError::Unimplemented("load failure")));

  {
    let mut scope = heap.scope();
    let promise =
      load_requested_modules(&mut vm, &mut scope, &mut modules, &mut host, a, HostDefined::default())
        .unwrap();
    scope.push_root(promise).unwrap();

    let Value::Object(promise_obj) = promise else {
      panic!("LoadRequestedModules should return a Promise object");
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj).unwrap(),
      PromiseState::Rejected
    );

    // Completion of unrelated outstanding loads should be ignored (no panics, no status changes).
    assert_eq!(host.pending.len(), 1);
    host.complete_pending(&mut vm, &mut scope, &mut modules, 0);
    assert_eq!(
      scope.heap().promise_state(promise_obj).unwrap(),
      PromiseState::Rejected
    );

    assert_eq!(modules.module(a).status, ModuleStatus::New);
    assert_eq!(modules.module(b).status, ModuleStatus::New);
    assert_eq!(modules.module(c).status, ModuleStatus::New);
  }

  realm.teardown(&mut heap);
}

#[test]
fn unsupported_import_attributes_reject_with_syntax_error() {
  let mut modules = ModuleGraph::new();
  let b = modules.add_module(record(Vec::new())).expect("add module");
  let a = modules
    .add_module(record(vec![req_with_attr("B", "type", "json")]))
    .expect("add module");

  let (mut vm, mut heap, mut realm) = new_vm_and_heap();
  let mut host = FakeHost::default();

  {
    let mut scope = heap.scope();
    let promise =
      load_requested_modules(&mut vm, &mut scope, &mut modules, &mut host, a, HostDefined::default())
        .unwrap();
    scope.push_root(promise).unwrap();

    let Value::Object(promise_obj) = promise else {
      panic!("LoadRequestedModules should return a Promise object");
    };

    assert_eq!(
      scope.heap().promise_state(promise_obj).unwrap(),
      PromiseState::Rejected
    );
    assert!(host.pending.is_empty(), "host loader should not have been invoked");

    let Some(result) = scope.heap().promise_result(promise_obj).unwrap() else {
      panic!("expected rejected promise to have a result");
    };
    let Value::Object(err_obj) = result else {
      panic!("expected promise rejection result to be an object");
    };

    let name_key = PropertyKey::from_string(scope.alloc_string("name").unwrap());
    let Some(desc) = scope.heap().object_get_own_property(err_obj, &name_key).unwrap() else {
      panic!("expected SyntaxError object to have a 'name' property");
    };
    let PropertyKind::Data { value, .. } = desc.kind else {
      panic!("expected SyntaxError.name to be a data property");
    };
    let Value::String(name) = value else {
      panic!("expected SyntaxError.name to be a string");
    };
    assert_eq!(scope.heap().get_string(name).unwrap().to_utf8_lossy(), "SyntaxError");

    assert_eq!(modules.module(a).status, ModuleStatus::New);
    assert_eq!(modules.module(b).status, ModuleStatus::New);
  }

  realm.teardown(&mut heap);
}

struct HostSupportingType(FakeHost);

impl HostSupportingType {
  fn plan_sync(&mut self, specifier: &str, result: ModuleCompletion) {
    self.0.plan_sync(specifier, result);
  }
}

impl VmHostHooks for HostSupportingType {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<vm_js::RealmId>) {
    self.0.host_enqueue_promise_job(job, realm);
  }

  fn host_call_job_callback(
    &mut self,
    ctx: &mut dyn VmJobContext,
    callback: &JobCallback,
    this_argument: Value,
    arguments: &[Value],
  ) -> Result<Value, VmError> {
    self
      .0
      .host_call_job_callback(ctx, callback, this_argument, arguments)
  }

  fn host_load_imported_module(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    host_defined: HostDefined,
    payload: ModuleLoadPayload,
  ) -> Result<(), VmError> {
    self.0.host_load_imported_module(
      vm,
      scope,
      modules,
      referrer,
      module_request,
      host_defined,
      payload,
    )
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    static SUPPORTED: [&str; 1] = ["type"];
    &SUPPORTED
  }
}

#[test]
fn supported_import_attributes_allow_module_loading() {
  let mut modules = ModuleGraph::new();
  let b = modules.add_module(record(Vec::new())).expect("add module");
  let a = modules
    .add_module(record(vec![req_with_attr("B", "type", "json")]))
    .expect("add module");

  let (mut vm, mut heap, mut realm) = new_vm_and_heap();

  let mut host = HostSupportingType(FakeHost::default());
  host.plan_sync("B", Ok(b));

  {
    let mut scope = heap.scope();
    let promise =
      load_requested_modules(&mut vm, &mut scope, &mut modules, &mut host, a, HostDefined::default())
        .unwrap();
    scope.push_root(promise).unwrap();

    let Value::Object(promise_obj) = promise else {
      panic!("LoadRequestedModules should return a Promise object");
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj).unwrap(),
      PromiseState::Fulfilled
    );
    assert!(host.0.pending.is_empty());
  }

  realm.teardown(&mut heap);
}
