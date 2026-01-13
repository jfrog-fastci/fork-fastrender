use std::any::Any;
use std::collections::HashMap;

use vm_js::{
  CompiledScript, Heap, HeapLimits, HostDefined, ImportMetaProperty, Job, JsString, MicrotaskQueue,
  ModuleGraph, ModuleId, ModuleLoadPayload, ModuleReferrer, ModuleRequest, PromiseState, PropertyKey,
  Realm, Scope, SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext,
  VmOptions,
};

fn new_vm_heap_realm() -> Result<(Vm, Heap, Realm), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let realm = Realm::new(&mut vm, &mut heap)?;
  Ok((vm, heap, realm))
}

fn add_compiled_module_with_specifier(
  graph: &mut ModuleGraph,
  heap: &mut Heap,
  specifier: &str,
  source: &str,
) -> Result<ModuleId, VmError> {
  let script = CompiledScript::compile_module(heap, specifier, source)?;
  let mut record = SourceTextModuleRecord::parse_source(script.source.clone())?;
  record.compiled = Some(script);
  graph.add_module_with_specifier(specifier, record)
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
  scope.get_with_host_and_hooks(vm, host, hooks, ns, key, Value::Object(ns))
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
  modules: HashMap<JsString, ModuleId>,
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
    self
      .modules
      .insert(JsString::from_str(specifier).unwrap(), module);
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
    let spec = JsString::from_str(specifier).unwrap();
    let idx = self
      .pending
      .iter()
      .position(|p| p.request.specifier == spec)
      .ok_or_else(|| VmError::InvariantViolation("no pending module load for specifier"))?;
    let pending = self.pending.remove(idx);

    let module = *self
      .modules
      .get(&spec)
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
      &mut heap,
      r#"
        export const before = import.meta.url;
        await Promise.resolve();
        export const after = import.meta.url;
      "#,
    )?,
  )?;
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
fn import_meta_works_after_top_level_await_in_compiled_module() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = add_compiled_module_with_specifier(
    &mut graph,
    &mut heap,
    "m.js",
    r#"
      export const before = import.meta.url;
      await Promise.resolve();
      export const after = import.meta.url;
    "#,
  )?;
  graph.link_all_by_specifier();

  // Link once so instantiation can use the parse tree, then discard the AST to ensure top-level
  // await evaluation parses on demand and retains the AST across suspension.
  graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), m)?;
  graph.module_mut(m).ast = None;
  // The compiled script retains the `SourceText`; ensure evaluation does not require
  // `SourceTextModuleRecord::source` once compilation has happened.
  graph.module_mut(m).source = None;

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

  let eval_promise_obj = match eval_promise {
    Value::Object(obj) => obj,
    _ => return Err(VmError::InvariantViolation("module evaluation must return a promise object")),
  };
  assert_eq!(heap.promise_state(eval_promise_obj)?, PromiseState::Pending);

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
  let Value::String(before) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "before")? else {
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
fn compiled_module_top_level_await_falls_back_to_ast() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = add_compiled_module_with_specifier(
    &mut graph,
    &mut heap,
    "m.js",
    r#"
      await Promise.resolve(1);
      export const x = 2;
    "#,
  )?;
  graph.link_all_by_specifier();

  graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), m)?;
  graph.module_mut(m).ast = None;
  graph.module_mut(m).source = None;

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
  assert_eq!(heap.promise_state(eval_promise_obj)?, PromiseState::Pending);

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
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "x")?,
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
fn import_meta_works_in_async_class_method_called_from_promise_job() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export let ok = 0;

        // Ensure this class definition runs through the async evaluator.
        await Promise.resolve();

        class C {
          static m() {
            return import.meta.url === "https://example.invalid/m.js";
          }
        }

        // Call the class method from a Promise job (no active execution context).
        Promise.resolve()
          .then(C.m)
          .then(v => { ok = v ? 1 : 0; })
          .catch(() => { ok = -1; });
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  // `import.meta` from Promise jobs resolves via `Vm::module_graph_ptr()`. Real embeddings attach
  // their module graph to the VM; tests that invoke module code from jobs must do the same.
  vm.set_module_graph(&mut graph);

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

  let res = (|| -> Result<(), VmError> {
    hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

    {
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
        ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "ok")?,
        Value::Number(1.0)
      );
    }

    Ok(())
  })();

  heap.remove_root(eval_promise_root);
  graph.abort_tla_evaluation(&mut vm, &mut heap, m);
  hooks.teardown_jobs(&mut vm, &mut heap);
  graph.teardown(&mut vm, &mut heap);
  vm.clear_module_graph();
  realm.teardown(&mut heap);
  res
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
      &mut heap,
      r#"
        export default await Promise.resolve(1);
        export const ok = 2;
      "#,
    )?,
  )?;
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
fn class_static_block_runs_during_async_module_evaluation() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        await Promise.resolve();
        class C {
          static {
            globalThis.__ran = 1;
          }
        }
        export default globalThis.__ran;
      "#,
    )?,
  )?;
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

  drop(scope);
  heap.remove_root(eval_promise_root);
  graph.abort_tla_evaluation(&mut vm, &mut heap, m);
  hooks.teardown_jobs(&mut vm, &mut heap);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

// Exercises `IncrementModuleAsyncEvaluationCount` + `[[AsyncEvaluationOrder]]` assignment using the
// spec's "asynchronous cyclic module graph" example.
//
// See: ECMA-262 Figure "An asynchronous cyclic module graph" + Table "Module fields after the
// initial Evaluate() call".
#[test]
fn module_async_evaluation_order_is_deterministic() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let mut graph = ModuleGraph::new();
  let a = graph.add_module_with_specifier(
    "a.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import "b.js";
        import "c.js";
        export {};
      "#,
    )?,
  )?;
  let b = graph.add_module_with_specifier(
    "b.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import "d.js";
        await 0;
        export {};
      "#,
    )?,
  )?;
  let c = graph.add_module_with_specifier(
    "c.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import "d.js";
        import "e.js";
        export {};
      "#,
    )?,
  )?;
  let d = graph.add_module_with_specifier(
    "d.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import "a.js";
        await 0;
        export {};
      "#,
    )?,
  )?;
  let e = graph.add_module_with_specifier(
    "e.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        await 0;
        export {};
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  graph.inner_module_evaluation(&mut vm, a)?;

  let order_a = graph
    .module_async_evaluation_order(a)
    .expect("a should have an async evaluation order");
  let order_b = graph
    .module_async_evaluation_order(b)
    .expect("b should have an async evaluation order");
  let order_c = graph
    .module_async_evaluation_order(c)
    .expect("c should have an async evaluation order");
  let order_d = graph
    .module_async_evaluation_order(d)
    .expect("d should have an async evaluation order");
  let order_e = graph
    .module_async_evaluation_order(e)
    .expect("e should have an async evaluation order");

  // The spec example assigns the following relative order:
  // D < B < E < C < A.
  assert!(order_d < order_b);
  assert!(order_b < order_e);
  assert!(order_e < order_c);
  assert!(order_c < order_a);

  // Ensure `AsyncModuleExecutionFulfilled` produces a deterministically sorted execList when
  // multiple ancestors become available at once:
  // - E fulfilling first decrements C to 1 pending dependency (no execList yet),
  // - D fulfilling next makes both B and C available, and sorting must execute B before C.
  let exec_after_e = graph.async_module_execution_fulfilled(e)?;
  assert!(exec_after_e.is_empty());

  let exec_after_d = graph.async_module_execution_fulfilled(d)?;
  assert_eq!(exec_after_d, vec![b, c]);

  Ok(())
}

#[test]
fn module_async_evaluation_order_is_repeatable_on_same_vm() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let mut graph = ModuleGraph::new();
  let a = graph.add_module_with_specifier(
    "a.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import "b.js";
        export {};
      "#,
    )?,
  )?;
  let b = graph.add_module_with_specifier(
    "b.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        await 0;
        export {};
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  graph.inner_module_evaluation(&mut vm, a)?;
  let order_a_1 = graph
    .module_async_evaluation_order(a)
    .expect("a should have an async evaluation order");
  let order_b_1 = graph
    .module_async_evaluation_order(b)
    .expect("b should have an async evaluation order");
  assert_ne!(order_a_1, order_b_1);

  // Re-run `InnerModuleEvaluation` on the same VM/graph and ensure the per-module integer orders
  // are stable, rather than drifting due to the VM's `[[ModuleAsyncEvaluationCount]]`.
  graph.inner_module_evaluation(&mut vm, a)?;
  let order_a_2 = graph
    .module_async_evaluation_order(a)
    .expect("a should have an async evaluation order (2nd run)");
  let order_b_2 = graph
    .module_async_evaluation_order(b)
    .expect("b should have an async evaluation order (2nd run)");

  assert_eq!(order_a_1, order_a_2);
  assert_eq!(order_b_1, order_b_2);

  Ok(())
}

#[test]
fn throw_await_rejects_module_evaluation_promise() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        await Promise.resolve();
        throw await Promise.resolve('boom');
        export const unreachable = 1;
      "#,
    )?,
  )?;
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

  let scope = heap.scope();
  let eval_promise_value = scope
    .heap()
    .get_root(eval_promise_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let Value::Object(eval_promise_obj) = eval_promise_value else {
    return Err(VmError::InvariantViolation("evaluation promise root must reference an object"));
  };
  assert_eq!(scope.heap().promise_state(eval_promise_obj)?, PromiseState::Rejected);

  let reason = scope
    .heap()
    .promise_result(eval_promise_obj)?
    .expect("rejected promise should have a reason");
  let Value::String(reason_s) = reason else {
    return Err(VmError::InvariantViolation(
      "expected module evaluation rejection reason to be a string",
    ));
  };
  assert_eq!(scope.heap().get_string(reason_s)?.to_utf8_lossy(), "boom");

  drop(scope);
  heap.remove_root(eval_promise_root);
  graph.abort_tla_evaluation(&mut vm, &mut heap, m);
  hooks.teardown_jobs(&mut vm, &mut heap);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn throw_await_error_object_attaches_throw_site_stack() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      "const err = new Error('boom');\nawait Promise.resolve();\nthrow await Promise.resolve(err);\nexport const unreachable = 1;",
    )?,
  )?;
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

  let eval_promise_obj = match eval_promise {
    Value::Object(obj) => obj,
    _ => return Err(VmError::InvariantViolation("module evaluation must return a promise object")),
  };
  assert_eq!(heap.promise_state(eval_promise_obj)?, PromiseState::Pending);

  hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let eval_promise_value = scope
    .heap()
    .get_root(eval_promise_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let Value::Object(eval_promise_obj) = eval_promise_value else {
    return Err(VmError::InvariantViolation("evaluation promise root must reference an object"));
  };
  assert_eq!(scope.heap().promise_state(eval_promise_obj)?, PromiseState::Rejected);

  let reason = scope
    .heap()
    .promise_result(eval_promise_obj)?
    .expect("rejected promise should have a reason");
  let Value::Object(err_obj) = reason else {
    return Err(VmError::InvariantViolation(
      "expected module evaluation rejection reason to be an object",
    ));
  };

  scope.push_root(Value::Object(err_obj))?;
  let key_s = scope.alloc_string("stack")?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  let Value::String(stack_s) = scope
    .heap()
    .object_get_own_data_property_value(err_obj, &key)?
    .unwrap_or(Value::Undefined)
  else {
    return Err(VmError::InvariantViolation(
      "expected rejection Error object to have a string `stack` property",
    ));
  };

  // The `throw await` statement is the 3rd line of the module and starts at column 1.
  let stack = scope.heap().get_string(stack_s)?.to_utf8_lossy();
  let first_frame = stack.lines().find(|line| line.starts_with("at ")).unwrap_or("");
  assert!(
    first_frame.starts_with("at <inline>:3:1"),
    "unexpected stack trace: {stack:?}"
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
    SourceTextModuleRecord::parse(&mut heap, "export const x = 1;")?,
  )?;
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        await Promise.resolve();
        export const p = import('./dep.js');
      "#,
    )?,
  )?;
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
  assert_eq!(
    hooks.pending[0].request.specifier,
    JsString::from_str("./dep.js").unwrap()
  );
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

#[test]
fn for_await_of_and_await_in_initializer_work_in_modules() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export const init = await Promise.resolve(2);
        export let out = 0;
        for await (const x of [Promise.resolve(1)]) { out = x; }
      "#,
    )?,
  )?;
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

  assert_eq!(
    vm.async_continuation_count(),
    1,
    "pending TLA evaluation should store exactly one async continuation"
  );

  hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  assert_eq!(
    vm.async_continuation_count(),
    0,
    "module TLA continuation should be cleaned up after evaluation completes"
  );

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
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "init")?,
    Value::Number(2.0)
  );
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "out")?,
    Value::Number(1.0)
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
fn top_level_await_in_for_of_lhs_destructuring_default_value_executes() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export let out = "bad";
        for (const { x = await Promise.resolve("ok") } of [ {} ]) { out = x; }
      "#,
    )?,
  )?;
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
  assert_eq!(heap.promise_state(eval_promise_obj)?, PromiseState::Pending);
  assert_eq!(vm.async_continuation_count(), 1);

  hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  assert_eq!(vm.async_continuation_count(), 0);

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
  let out = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "out")?;
  let Value::String(out_s) = out else {
    return Err(VmError::InvariantViolation("expected module export 'out' to be a string"));
  };
  assert_eq!(scope.heap().get_string(out_s)?.to_utf8_lossy(), "ok");

  drop(scope);
  heap.remove_root(eval_promise_root);
  graph.abort_tla_evaluation(&mut vm, &mut heap, m);
  hooks.teardown_jobs(&mut vm, &mut heap);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn await_rejection_is_catchable_in_modules() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export let out = "unset";
        try {
          await Promise.reject("boom");
          out = "unreachable";
        } catch (e) {
          out = e;
        }
      "#,
    )?,
  )?;
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
  assert_eq!(heap.promise_state(eval_promise_obj)?, PromiseState::Pending);
  assert_eq!(vm.async_continuation_count(), 1);

  hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  assert_eq!(vm.async_continuation_count(), 0);

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
  let out = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "out")?;
  let Value::String(out_s) = out else {
    return Err(VmError::InvariantViolation("expected module export 'out' to be a string"));
  };
  assert_eq!(scope.heap().get_string(out_s)?.to_utf8_lossy(), "boom");

  drop(scope);
  heap.remove_root(eval_promise_root);
  graph.abort_tla_evaluation(&mut vm, &mut heap, m);
  hooks.teardown_jobs(&mut vm, &mut heap);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn await_promise_resolve_constructor_getter_throw_is_catchable_in_modules() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export let out = "unset";
        const p = Promise.resolve(1);
        Object.defineProperty(p, "constructor", { get() { throw "boom"; } });
        try {
          await p;
          out = "unreachable";
        } catch (e) {
          out = e;
        }
      "#,
    )?,
  )?;
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

  let eval_promise_obj = match eval_promise {
    Value::Object(obj) => obj,
    _ => return Err(VmError::InvariantViolation("module evaluation must return a promise object")),
  };
  assert_eq!(heap.promise_state(eval_promise_obj)?, PromiseState::Fulfilled);
  assert_eq!(vm.async_continuation_count(), 0);

  // Drain a checkpoint (no-op expected, but matches embedding behavior).
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
  let out = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "out")?;
  let Value::String(out_s) = out else {
    return Err(VmError::InvariantViolation("expected module export 'out' to be a string"));
  };
  assert_eq!(scope.heap().get_string(out_s)?.to_utf8_lossy(), "boom");

  drop(scope);
  heap.remove_root(eval_promise_root);
  graph.abort_tla_evaluation(&mut vm, &mut heap, m);
  hooks.teardown_jobs(&mut vm, &mut heap);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn multiple_top_level_awaits_reuse_continuation_without_leaking() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = TestHostHooks::new("https://example.invalid/m.js");
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export let resolve1;
        export let resolve2;
        export const p1 = new Promise(r => resolve1 = r);
        export const p2 = new Promise(r => resolve2 = r);
        export let out = 0;
        await p1;
        out = 1;
        await p2;
        out = 2;
      "#,
    )?,
  )?;
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

  let eval_promise_obj = match eval_promise {
    Value::Object(obj) => obj,
    _ => return Err(VmError::InvariantViolation("module evaluation must return a promise object")),
  };
  assert_eq!(heap.promise_state(eval_promise_obj)?, PromiseState::Pending);

  // Should have stored one async continuation for the first suspension.
  assert_eq!(vm.async_continuation_count(), 1);

  // Resolve the first awaited promise (p1).
  {
    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    let resolve1 = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "resolve1")?;
    scope.push_root(resolve1)?;
    let _ = vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      resolve1,
      Value::Undefined,
      &[Value::Undefined],
    )?;
  }

  hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  // Module should have suspended again at the second `await`, still using a single continuation id.
  assert_eq!(heap.promise_state(eval_promise_obj)?, PromiseState::Pending);
  assert_eq!(vm.async_continuation_count(), 1);

  // Resolve the second awaited promise (p2).
  {
    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    let resolve2 = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "resolve2")?;
    scope.push_root(resolve2)?;
    let _ = vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      resolve2,
      Value::Undefined,
      &[Value::Undefined],
    )?;
  }

  hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  assert_eq!(vm.async_continuation_count(), 0);

  {
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
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "out")?,
      Value::Number(2.0)
    );
  }

  heap.remove_root(eval_promise_root);
  graph.abort_tla_evaluation(&mut vm, &mut heap, m);
  hooks.teardown_jobs(&mut vm, &mut heap);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}
