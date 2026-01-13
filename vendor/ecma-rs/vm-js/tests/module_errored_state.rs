use std::collections::VecDeque;

use vm_js::{
  Heap, HeapLimits, Job, ModuleGraph, PromiseState, Realm, RootId, SourceTextModuleRecord, Value,
  Vm, VmError, VmHostHooks, VmJobContext, VmOptions,
};

#[derive(Default)]
struct Hooks {
  jobs: VecDeque<Job>,
}

impl VmHostHooks for Hooks {
  fn host_enqueue_promise_job(&mut self, job: Job, _realm: Option<vm_js::RealmId>) {
    self.jobs.push_back(job);
  }
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

fn discard_jobs(vm: &mut Vm, heap: &mut Heap, hooks: &mut Hooks) {
  let mut ctx = JobCtx { vm, heap };
  while let Some(job) = hooks.jobs.pop_front() {
    job.discard(&mut ctx);
  }
}

fn boom_module(heap: &mut Heap) -> SourceTextModuleRecord {
  SourceTextModuleRecord::parse(heap, "throw new Error('boom');").expect("module should parse")
}

#[test]
fn sync_evaluation_rethrows_cached_error_object() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let global_object = realm.global_object();
  let realm_id = realm.id();

  let mut modules = ModuleGraph::new();
  let module = modules.add_module(boom_module(&mut heap))?;

  let mut host = ();
  let mut hooks = Hooks::default();

  let err1 = {
    let mut scope = heap.scope();
    modules
      .evaluate_sync_with_scope(
        &mut vm,
        &mut scope,
        global_object,
        realm_id,
        module,
        &mut host,
        &mut hooks,
      )
      .expect_err("module should throw")
  };
  discard_jobs(&mut vm, &mut heap, &mut hooks);

  let thrown1 = err1.thrown_value().expect("expected a thrown value");
  let Value::Object(obj1) = thrown1 else {
    panic!("expected an object throw value, got {thrown1:?}");
  };

  // The cached module error value should keep the thrown object alive across GC.
  heap.collect_garbage();
  assert!(heap.is_valid_object(obj1));

  let err2 = {
    let mut scope = heap.scope();
    modules
      .evaluate_sync_with_scope(
        &mut vm,
        &mut scope,
        global_object,
        realm_id,
        module,
        &mut host,
        &mut hooks,
      )
      .expect_err("module should deterministically rethrow")
  };
  discard_jobs(&mut vm, &mut heap, &mut hooks);

  let thrown2 = err2.thrown_value().expect("expected a thrown value");
  let Value::Object(obj2) = thrown2 else {
    panic!("expected an object throw value, got {thrown2:?}");
  };

  assert_eq!(
    obj1, obj2,
    "subsequent evaluation should throw the same Error object identity"
  );

  modules.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn evaluation_promise_rejects_with_cached_error_object() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let global_object = realm.global_object();
  let realm_id = realm.id();

  let mut modules = ModuleGraph::new();
  let module = modules.add_module(boom_module(&mut heap))?;

  let mut host = ();
  let mut hooks = Hooks::default();

  let reason1 = {
    let mut scope = heap.scope();
    let promise = modules.evaluate_with_scope(
      &mut vm,
      &mut scope,
      global_object,
      realm_id,
      module,
      &mut host,
      &mut hooks,
    )?;

    let Value::Object(promise_obj) = promise else {
      panic!("expected module evaluation to return a Promise object");
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj)?,
      PromiseState::Rejected,
      "module evaluation promise should be rejected"
    );
    scope
      .heap()
      .promise_result(promise_obj)?
      .expect("promise should have a rejection reason")
  };
  discard_jobs(&mut vm, &mut heap, &mut hooks);

  let Value::Object(obj1) = reason1 else {
    panic!("expected promise rejection reason to be an object, got {reason1:?}");
  };

  // The cached module error value should keep the thrown object alive across GC.
  heap.collect_garbage();
  assert!(heap.is_valid_object(obj1));

  let reason2 = {
    let mut scope = heap.scope();
    let promise = modules.evaluate_with_scope(
      &mut vm,
      &mut scope,
      global_object,
      realm_id,
      module,
      &mut host,
      &mut hooks,
    )?;

    let Value::Object(promise_obj) = promise else {
      panic!("expected module evaluation to return a Promise object");
    };
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Rejected);
    scope
      .heap()
      .promise_result(promise_obj)?
      .expect("promise should have a rejection reason")
  };
  discard_jobs(&mut vm, &mut heap, &mut hooks);

  let Value::Object(obj2) = reason2 else {
    panic!("expected promise rejection reason to be an object, got {reason2:?}");
  };

  assert_eq!(
    obj1, obj2,
    "subsequent evaluation should reject with the same Error object identity"
  );

  modules.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}
