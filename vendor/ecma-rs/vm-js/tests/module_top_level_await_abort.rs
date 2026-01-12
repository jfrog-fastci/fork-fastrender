use vm_js::{
  Heap, HeapLimits, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey, Realm, RootId, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHostHooks, VmJobContext, VmOptions,
};

fn new_vm_heap_realm() -> Result<(Vm, Heap, Realm), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let realm = Realm::new(&mut vm, &mut heap)?;
  Ok((vm, heap, realm))
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
    self.heap.remove_root(id);
  }
}

fn get_error_message(scope: &mut Scope<'_>, err_obj: vm_js::GcObject) -> Result<Option<String>, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(err_obj))?;
  let key_s = scope.alloc_string("message")?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  let value = scope
    .heap()
    .object_get_own_data_property_value(err_obj, &key)?
    .unwrap_or(Value::Undefined);
  match value {
    Value::String(s) => Ok(Some(scope.heap().get_string(s)?.to_utf8_lossy())),
    _ => Ok(None),
  }
}

#[test]
fn abort_tla_evaluation_rejects_pending_module_evaluation_promise() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let entry = graph.add_module(SourceTextModuleRecord::parse(
    "await new Promise(() => {}); export {};",
  )?);

  let promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    entry,
    &mut host,
    &mut hooks,
  )?;

  let Value::Object(promise_obj) = promise else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };

  {
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Pending);
  }

  // Drain a microtask checkpoint; the awaited promise never resolves, so evaluation should still be
  // pending.
  {
    let mut ctx = JobCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    let errors = hooks.perform_microtask_checkpoint(&mut ctx);
    assert!(errors.is_empty(), "unexpected microtask errors: {errors:?}");
  }

  {
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Pending);
  }

  graph.abort_tla_evaluation(&mut vm, &mut heap, entry);

  {
    let mut scope = heap.scope();
    scope.push_root(promise)?;

    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Rejected);
    let reason = scope.heap().promise_result(promise_obj)?.unwrap_or(Value::Undefined);
    let Value::Object(err_obj) = reason else {
      panic!("expected abort_tla_evaluation to reject with an Error object, got {reason:?}");
    };

    let msg = get_error_message(&mut scope, err_obj)?
      .unwrap_or_else(|| "<non-string message>".to_string());
    assert!(
      msg.contains("asynchronous module loading/evaluation is not supported"),
      "unexpected rejection message: {msg:?}"
    );
  }

  // Aborting should not leave queued jobs behind on either the host hook microtask queue or the VM
  // fallback microtask queue.
  assert!(hooks.is_empty());
  assert!(vm.microtask_queue().is_empty());

  // Abort should have restored the module graph pointer (the module graph was only attached for the
  // duration of module evaluation).
  assert!(vm.module_graph_ptr().is_none());

  realm.teardown(&mut heap);
  Ok(())
}

