use std::any::Any;
use std::collections::HashMap;

use vm_js::{
  Heap, HeapLimits, HostDefined, ImportMetaProperty, Job, MicrotaskQueue, ModuleGraph, ModuleId,
  ModuleLoadPayload, ModuleReferrer, ModuleRequest, PromiseState, PropertyKey, Realm, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext, VmOptions,
};

fn new_vm_heap_realm() -> Result<(Vm, Heap, Realm), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let realm = Realm::new(&mut vm, &mut heap)?;
  Ok((vm, heap, realm))
}

fn ns_get(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  ns: vm_js::GcObject,
  name: &str,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(ns))?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.ordinary_get_with_host_and_hooks(vm, host, hooks, ns, key, Value::Object(ns))
}

#[derive(Debug)]
struct PendingLoad {
  referrer: ModuleReferrer,
  request: ModuleRequest,
  payload: ModuleLoadPayload,
}

/// Combined host hooks for top-level await tests.
///
/// - Routes Promise jobs into a host-owned [`MicrotaskQueue`].
/// - Captures dynamic import load requests so tests can complete them manually.
/// - Implements `import.meta` hooks to provide a stable `import.meta.url`.
struct TestHostHooks {
  microtasks: MicrotaskQueue,

  // `import.meta` customization.
  url: String,
  import_meta_get_calls: u32,
  import_meta_finalize_calls: u32,

  // Dynamic import capture.
  modules: HashMap<String, ModuleId>,
  pending: Vec<PendingLoad>,
}

impl TestHostHooks {
  fn new(url: &str) -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      url: url.to_string(),
      import_meta_get_calls: 0,
      import_meta_finalize_calls: 0,
      modules: HashMap::new(),
      pending: Vec::new(),
    }
  }

  fn register_module(&mut self, specifier: &str, module: ModuleId) {
    self.modules.insert(specifier.to_string(), module);
  }

  fn pending_count(&self) -> usize {
    self.pending.len()
  }

  fn complete_load_for(
    &mut self,
    vm: &mut Vm,
    heap: &mut Heap,
    modules: &mut ModuleGraph,
    specifier: &str,
  ) -> Result<(), VmError> {
    let idx = self
      .pending
      .iter()
      .position(|p| p.request.specifier == specifier)
      .ok_or_else(|| VmError::InvariantViolation("no pending module load for specifier"))?;
    let pending = self.pending.remove(idx);

    let module = *self
      .modules
      .get(specifier)
      .ok_or_else(|| VmError::InvariantViolation("no module registered for specifier"))?;

    let mut scope = heap.scope();
    vm.finish_loading_imported_module(
      &mut scope,
      modules,
      self,
      pending.referrer,
      pending.request,
      pending.payload,
      Ok(module),
    )?;
    Ok(())
  }

  fn perform_microtask_checkpoint(&mut self, vm: &mut Vm, heap: &mut Heap) -> Result<(), VmError> {
    if !self.microtasks.begin_checkpoint() {
      return Ok(());
    }

    struct Ctx<'a> {
      vm: &'a mut Vm,
      heap: &'a mut Heap,
    }

    impl VmJobContext for Ctx<'_> {
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

      fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: vm_js::RootId) {
        self.heap.remove_root(id)
      }
    }

    let mut ctx = Ctx { vm, heap };
    let mut errors = Vec::<VmError>::new();
    while let Some((_realm, job)) = self.microtasks.pop_front() {
      if let Err(err) = job.run(&mut ctx, self) {
        let is_termination = matches!(err, VmError::Termination(_));
        errors.push(err);
        if is_termination {
          // Termination is a hard stop; discard remaining queued jobs so we don't leak persistent
          // roots.
          self.microtasks.teardown(&mut ctx);
          break;
        }
      }
    }
    self.microtasks.end_checkpoint();

    if let Some(err) = errors.into_iter().next() {
      return Err(err);
    }
    Ok(())
  }

  fn teardown_jobs(&mut self, vm: &mut Vm, heap: &mut Heap) {
    struct Ctx<'a> {
      vm: &'a mut Vm,
      heap: &'a mut Heap,
    }

    impl VmJobContext for Ctx<'_> {
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

      fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: vm_js::RootId) {
        self.heap.remove_root(id)
      }
    }

    let mut ctx = Ctx { vm, heap };
    self.microtasks.teardown(&mut ctx);
  }
}

impl VmHostHooks for TestHostHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<vm_js::RealmId>) {
    self.microtasks.host_enqueue_promise_job(job, realm);
  }

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    Some(self)
  }

  fn host_get_import_meta_properties(
    &mut self,
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _module: vm_js::ModuleId,
  ) -> Result<Vec<ImportMetaProperty>, VmError> {
    self.import_meta_get_calls += 1;

    // Root across subsequent allocations in case they trigger GC.
    let url_key = scope.alloc_string("url")?;
    scope.push_root(Value::String(url_key))?;
    let url_value = scope.alloc_string(&self.url)?;
    scope.push_root(Value::String(url_value))?;

    Ok(vec![ImportMetaProperty {
      key: PropertyKey::from_string(url_key),
      value: Value::String(url_value),
    }])
  }

  fn host_finalize_import_meta(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _import_meta: vm_js::GcObject,
    _module: vm_js::ModuleId,
  ) -> Result<(), VmError> {
    self.import_meta_finalize_calls += 1;
    Ok(())
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    &[]
  }

  fn host_load_imported_module(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _modules: &mut ModuleGraph,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    _host_defined: HostDefined,
    payload: ModuleLoadPayload,
  ) -> Result<(), VmError> {
    self.pending.push(PendingLoad {
      referrer,
      request: module_request,
      payload,
    });
    Ok(())
  }
}

#[test]
fn import_meta_works_after_top_level_await() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      r#"
        export const before = import.meta.url;
        await Promise.resolve();
        export const after = import.meta.url;
      "#,
    )?,
  );
  graph.link_all_by_specifier();

  let eval_promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    m,
    &mut host,
    &mut hooks,
  )?;
  let eval_promise_root = heap.add_root(eval_promise)?;

  // Top-level await should suspend module evaluation (promise starts pending).
  let eval_promise_obj = match eval_promise {
    Value::Object(obj) => obj,
    _ => return Err(VmError::InvariantViolation("module evaluation must return a promise object")),
  };
  assert_eq!(
    heap.promise_state(eval_promise_obj)?,
    PromiseState::Pending,
    "top-level await should produce a pending evaluation promise"
  );

  hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let eval_promise_value = scope
    .heap()
    .get_root(eval_promise_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let Value::Object(eval_promise_obj) = eval_promise_value else {
    return Err(VmError::InvariantViolation("evaluation promise root must reference an object"));
  };
  assert_eq!(scope.heap().promise_state(eval_promise_obj)?, PromiseState::Fulfilled);

  let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
  let Value::String(before) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "before")?
  else {
    return Err(VmError::InvariantViolation(
      "expected `before` export to be a string",
    ));
  };
  let Value::String(after) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "after")? else {
    return Err(VmError::InvariantViolation(
      "expected `after` export to be a string",
    ));
  };
  let before_s = scope.heap().get_string(before)?.to_utf8_lossy();
  let after_s = scope.heap().get_string(after)?.to_utf8_lossy();
  assert_eq!(before_s, "https://example.invalid/m.js");
  assert_eq!(after_s, "https://example.invalid/m.js");

  // `import.meta` must be cached per module even across top-level await resumption.
  assert_eq!(hooks.import_meta_get_calls, 1);
  assert_eq!(hooks.import_meta_finalize_calls, 1);

  drop(scope);
  heap.remove_root(eval_promise_root);
  graph.abort_tla_evaluation(&mut vm, &mut heap, m);
  hooks.teardown_jobs(&mut vm, &mut heap);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn export_default_await_initializes_default_binding() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      r#"
        export default await Promise.resolve(1);
        export const ok = 2;
      "#,
    )?,
  );
  graph.link_all_by_specifier();

  let eval_promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    m,
    &mut host,
    &mut hooks,
  )?;
  let eval_promise_root = heap.add_root(eval_promise)?;

  // Top-level await should suspend module evaluation (promise starts pending).
  let eval_promise_obj = match eval_promise {
    Value::Object(obj) => obj,
    _ => return Err(VmError::InvariantViolation("module evaluation must return a promise object")),
  };
  assert_eq!(
    heap.promise_state(eval_promise_obj)?,
    PromiseState::Pending,
    "top-level await should produce a pending evaluation promise"
  );

  hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let eval_promise_value = scope
    .heap()
    .get_root(eval_promise_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let Value::Object(eval_promise_obj) = eval_promise_value else {
    return Err(VmError::InvariantViolation("evaluation promise root must reference an object"));
  };
  assert_eq!(scope.heap().promise_state(eval_promise_obj)?, PromiseState::Fulfilled);

  let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "default")?,
    Value::Number(1.0)
  );
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "ok")?,
    Value::Number(2.0)
  );

  drop(scope);
  heap.remove_root(eval_promise_root);
  graph.abort_tla_evaluation(&mut vm, &mut heap, m);
  hooks.teardown_jobs(&mut vm, &mut heap);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn dynamic_import_after_top_level_await_starts_and_resolves() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let dep = graph.add_module_with_specifier(
    "./dep.js",
    SourceTextModuleRecord::parse("export const x = 1;")?,
  );
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      r#"
        await Promise.resolve();
        export const p = import('./dep.js');
      "#,
    )?,
  );
  graph.link_all_by_specifier();

  hooks.register_module("./dep.js", dep);

  let eval_promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    m,
    &mut host,
    &mut hooks,
  )?;
  let eval_promise_root = heap.add_root(eval_promise)?;

  // `import()` should not run until after the top-level await resumes.
  assert_eq!(hooks.pending_count(), 0);

  hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  // Dynamic import should have started during the resumed module evaluation.
  assert_eq!(hooks.pending_count(), 1);
  assert_eq!(hooks.pending[0].request.specifier, "./dep.js");
  assert_eq!(
    hooks.pending[0].referrer,
    ModuleReferrer::Module(m),
    "dynamic import referrer should be the active module, even after TLA resumption"
  );

  // Read `p` from the module namespace (should be a pending promise).
  let (p_root, p_obj) = {
    let mut scope = heap.scope();
    let ns_m = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    let p_value = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_m, "p")?;
    let Value::Object(promise_obj) = p_value else {
      return Err(VmError::InvariantViolation(
        "module export `p` should be a promise object",
      ));
    };
    let p_root = scope.heap_mut().add_root(p_value)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Pending);
    (p_root, promise_obj)
  };

  hooks.complete_load_for(&mut vm, &mut heap, &mut graph, "./dep.js")?;
  hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let p_value = scope
    .heap()
    .get_root(p_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let Value::Object(promise_obj) = p_value else {
    return Err(VmError::InvariantViolation("promise root should reference an object"));
  };
  assert_eq!(promise_obj, p_obj);
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

  let ns_value = scope
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  let Value::Object(ns_obj) = ns_value else {
    return Err(VmError::InvariantViolation(
      "dynamic import promise should fulfill to a namespace object",
    ));
  };

  assert!(matches!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_obj, "x")?,
    Value::Number(n) if n == 1.0
  ));

  drop(scope);
  heap.remove_root(p_root);
  heap.remove_root(eval_promise_root);
  graph.abort_tla_evaluation(&mut vm, &mut heap, m);
  hooks.teardown_jobs(&mut vm, &mut heap);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}
