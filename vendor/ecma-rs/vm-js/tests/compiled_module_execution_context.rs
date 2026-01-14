use std::collections::HashMap;

use vm_js::{
  CompiledScript, Heap, HeapLimits, HostDefined, ImportMetaProperty, Job, MicrotaskQueue, ModuleGraph,
  ModuleId, ModuleLoadPayload, ModuleReferrer, ModuleRequest, PromiseState, PropertyKey, Realm, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext, VmOptions,
};

fn compiled_module_record(
  heap: &mut Heap,
  specifier: &str,
  source: &str,
) -> Result<SourceTextModuleRecord, VmError> {
  let compiled = CompiledScript::compile_module(heap, specifier, source)?;
  let mut record = SourceTextModuleRecord::parse_source(heap, compiled.source.clone())?;
  record.compiled = Some(compiled);
  // Force ModuleGraph to use the compiled-module (HIR) instantiation + execution path.
  record.clear_ast();
  Ok(record)
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

/// Host hooks that provide:
/// - a microtask queue for Promise jobs
/// - `import.meta.url` via `host_get_import_meta_properties`
/// - dynamic `import()` completion via `host_load_imported_module`, while recording referrers
struct TestHostHooks {
  microtasks: MicrotaskQueue,
  modules: HashMap<String, ModuleId>,
  import_meta_urls: HashMap<ModuleId, String>,
  import_referrers: HashMap<String, ModuleReferrer>,
}

impl TestHostHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      modules: HashMap::new(),
      import_meta_urls: HashMap::new(),
      import_referrers: HashMap::new(),
    }
  }

  fn register_module(&mut self, specifier: &str, module: ModuleId) {
    self.modules.insert(specifier.to_string(), module);
  }

  fn register_import_meta_url(&mut self, module: ModuleId, url: &str) {
    self.import_meta_urls.insert(module, url.to_string());
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
        self.heap.remove_root(id);
      }
    }

    let mut ctx = Ctx { vm, heap };
    self.microtasks.teardown(&mut ctx);
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
        self.heap.remove_root(id);
      }
    }

    let mut ctx = Ctx { vm, heap };

    let mut first_err: Option<VmError> = None;
    let mut termination_err: Option<VmError> = None;
    while let Some((_realm, job)) = self.microtasks.pop_front() {
      let job_result = job.run(&mut ctx, self);
      match job_result {
        Ok(()) => {}
        Err(e @ VmError::Termination(_)) => {
          termination_err = Some(e);
          break;
        }
        Err(e) => {
          if first_err.is_none() {
            first_err = Some(e);
          }
        }
      }
    }

    if termination_err.is_some() {
      self.microtasks.teardown(&mut ctx);
    }

    self.microtasks.end_checkpoint();
    match termination_err {
      Some(e) => Err(e),
      None => first_err.map_or(Ok(()), Err),
    }
  }
}

impl VmHostHooks for TestHostHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<vm_js::RealmId>) {
    self.microtasks.host_enqueue_promise_job(job, realm);
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    &[]
  }

  fn host_get_import_meta_properties(
    &mut self,
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    module: ModuleId,
  ) -> Result<Vec<ImportMetaProperty>, VmError> {
    let meta_url = self
      .import_meta_urls
      .get(&module)
      .ok_or_else(|| VmError::InvariantViolation("no import.meta.url registered for module"))?;

    // Root across subsequent allocations in case they trigger GC.
    let url_key = scope.alloc_string("url")?;
    scope.push_root(Value::String(url_key))?;
    let url_value = scope.alloc_string(meta_url)?;
    scope.push_root(Value::String(url_value))?;

    Ok(vec![ImportMetaProperty {
      key: PropertyKey::from_string(url_key),
      value: Value::String(url_value),
    }])
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
    // `ModuleRequest` stores specifiers as `JsString` (UTF-16 code units). This host hook integration
    // uses UTF-8 `String` keys for lookup and bookkeeping; the test specifiers are ASCII so lossy
    // conversion is fine.
    let specifier = module_request.specifier_utf8_lossy();
    self
      .import_referrers
      .insert(specifier.clone(), referrer);

    let module = *self
      .modules
      .get(specifier.as_str())
      .ok_or_else(|| VmError::InvariantViolation("no module registered for specifier"))?;

    vm.finish_loading_imported_module(
      scope,
      modules,
      self,
      referrer,
      module_request,
      payload,
      Ok(module),
    )
  }
}

#[test]
fn compiled_module_import_meta_uses_callee_module_and_is_cached() -> Result<(), VmError> {
  const URL_A: &str = "https://example.invalid/a.js";
  const URL_B: &str = "https://example.invalid/b.js";

  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut modules = ModuleGraph::new();
  let a = modules.add_module_with_specifier(
    "a.js",
    compiled_module_record(
      &mut heap,
      "a.js",
      r#"
        export function get() { return import.meta.url; }
        export function getMeta() { return import.meta; }
      "#,
    )?,
  )?;

  let b = modules.add_module_with_specifier(
    "b.js",
    compiled_module_record(
      &mut heap,
      "b.js",
      r#"
        import { get, getMeta } from "a.js";
        export const url = get();
        export const cached = getMeta() === getMeta();
        export const scoped = getMeta() !== import.meta;
      "#,
    )?,
  )?;

  modules.link_all_by_specifier();

  let mut host_hooks = TestHostHooks::new();
  host_hooks.register_import_meta_url(a, URL_A);
  host_hooks.register_import_meta_url(b, URL_B);

  let mut dummy_host = ();

  let result: Result<(), VmError> = (|| {
    let eval_promise = modules.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut dummy_host,
      &mut host_hooks,
    )?;

    // Drain microtasks so the spec-visible evaluation promise settles.
    host_hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

    {
      let mut scope = heap.scope();
      scope.push_root(eval_promise)?;
      let Value::Object(promise_obj) = eval_promise else {
        return Err(VmError::InvariantViolation(
          "ModuleGraph::evaluate should return a Promise object",
        ));
      };
      assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
    }

    let mut scope = heap.scope();
    let ns_b = modules.get_module_namespace(b, &mut vm, &mut scope)?;

    let Value::String(url_s) = ns_get(&mut vm, &mut dummy_host, &mut host_hooks, &mut scope, ns_b, "url")?
    else {
      return Err(VmError::InvariantViolation("expected b.url to be a string"));
    };
    assert_eq!(scope.heap().get_string(url_s)?.to_utf8_lossy(), URL_A);

    assert_eq!(
      ns_get(
        &mut vm,
        &mut dummy_host,
        &mut host_hooks,
        &mut scope,
        ns_b,
        "cached"
      )?,
      Value::Bool(true)
    );
    assert_eq!(
      ns_get(
        &mut vm,
        &mut dummy_host,
        &mut host_hooks,
        &mut scope,
        ns_b,
        "scoped"
      )?,
      Value::Bool(true)
    );

    Ok(())
  })();

  modules.teardown(&mut vm, &mut heap);
  host_hooks.teardown_jobs(&mut vm, &mut heap);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_dynamic_import_referrer_uses_callee_module() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut modules = ModuleGraph::new();
  let a = modules.add_module_with_specifier(
    "a.js",
    compiled_module_record(
      &mut heap,
      "a.js",
      r#"
        export function load() { return import("./dep.js"); }
      "#,
    )?,
  )?;

  let b = modules.add_module_with_specifier(
    "b.js",
    compiled_module_record(
      &mut heap,
      "b.js",
      r#"
        import { load } from "a.js";
        export const p = load();
      "#,
    )?,
  )?;

  let dep = modules.add_module_with_specifier(
    "./dep.js",
    compiled_module_record(&mut heap, "./dep.js", "export const x = 1;")?,
  )?;

  modules.link_all_by_specifier();

  let mut host_hooks = TestHostHooks::new();
  host_hooks.register_module("./dep.js", dep);

  let mut dummy_host = ();

  let result: Result<(), VmError> = (|| {
    let _eval_promise = modules.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut dummy_host,
      &mut host_hooks,
    )?;

    // Read `p` from the module namespace (should be the Promise returned by dynamic `import()`).
    let p_root = {
      let mut scope = heap.scope();
      let ns_b = modules.get_module_namespace(b, &mut vm, &mut scope)?;
      let p_value = ns_get(&mut vm, &mut dummy_host, &mut host_hooks, &mut scope, ns_b, "p")?;

      let Value::Object(promise_obj) = p_value else {
        return Err(VmError::InvariantViolation(
          "module export p should be a promise object",
        ));
      };
      let root = scope.heap_mut().add_root(p_value)?;
      assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Pending);
      root
    };

    // Regression assertion: `import("./dep.js")` is evaluated inside a function whose
    // `[[ScriptOrModule]]` is module A, so `HostLoadImportedModule` must observe `referrer ==
    // Module(a)`, not module B.
    assert_eq!(
      host_hooks.import_referrers.get("./dep.js").copied(),
      Some(ModuleReferrer::Module(a))
    );

    // Drain microtasks so the dynamic import promise settles and queued jobs release their roots.
    host_hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

    // Verify the import promise fulfills to a namespace where `x === 1`.
    let mut scope = heap.scope();
    let p_value = scope
      .heap()
      .get_root(p_root)
      .ok_or_else(|| VmError::invalid_handle())?;
    let Value::Object(promise_obj) = p_value else {
      return Err(VmError::InvariantViolation(
        "promise root should reference an object",
      ));
    };
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

    let x_value = ns_get(&mut vm, &mut dummy_host, &mut host_hooks, &mut scope, ns_obj, "x")?;
    assert!(matches!(x_value, Value::Number(n) if n == 1.0));

    scope.heap_mut().remove_root(p_root);
    Ok(())
  })();

  modules.teardown(&mut vm, &mut heap);
  host_hooks.teardown_jobs(&mut vm, &mut heap);
  realm.teardown(&mut heap);

  result
}
