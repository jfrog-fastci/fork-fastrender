use std::collections::HashMap;

use vm_js::{
  Budget, Heap, HeapLimits, HostDefined, Job, JsRuntime, MicrotaskQueue, ModuleGraph, ModuleId,
  ModuleLoadPayload, ModuleReferrer, ModuleRequest, PromiseState, Scope, SourceText,
  SourceTextModuleRecord, TerminationReason, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext,
  VmOptions,
};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

struct TestHostHooks {
  microtasks: MicrotaskQueue,
  sources: HashMap<String, String>,
  modules: HashMap<String, ModuleId>,
  load_calls: Vec<String>,
}

impl TestHostHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      sources: HashMap::new(),
      modules: HashMap::new(),
      load_calls: Vec::new(),
    }
  }

  fn add_module_source(&mut self, specifier: &str, source: &str) {
    self.sources.insert(specifier.to_string(), source.to_string());
  }

  fn perform_microtask_checkpoint(
    &mut self,
    rt: &mut JsRuntime,
    host: &mut dyn VmHost,
  ) -> Result<(), VmError> {
    if !self.microtasks.begin_checkpoint() {
      return Ok(());
    }

    struct Ctx<'a> {
      vm: &'a mut Vm,
      heap: &'a mut Heap,
      host: &'a mut dyn VmHost,
    }

    impl VmJobContext for Ctx<'_> {
      fn call(
        &mut self,
        hooks: &mut dyn VmHostHooks,
        callee: Value,
        this: Value,
        args: &[Value],
      ) -> Result<Value, VmError> {
        let mut scope = self.heap.scope();
        self
          .vm
          .call_with_host_and_hooks(&mut *self.host, &mut scope, hooks, callee, this, args)
      }

      fn construct(
        &mut self,
        hooks: &mut dyn VmHostHooks,
        callee: Value,
        args: &[Value],
        new_target: Value,
      ) -> Result<Value, VmError> {
        let mut scope = self.heap.scope();
        self.vm.construct_with_host_and_hooks(
          &mut *self.host,
          &mut scope,
          hooks,
          callee,
          args,
          new_target,
        )
      }

      fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: vm_js::RootId) {
        self.heap.remove_root(id)
      }
    }

    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut ctx = Ctx { vm, heap, host };

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
    self.microtasks.enqueue_promise_job(job, realm);
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
    let _ = host_defined;
    let specifier = module_request.specifier_utf8_lossy();
    self.load_calls.push(specifier.clone());

    let module_id = match self.modules.get(&specifier).copied() {
      Some(id) => id,
      None => {
        let src = self
          .sources
          .get(&specifier)
          .ok_or_else(|| VmError::InvariantViolation("no source registered for module specifier"))?;
        let source =
          SourceText::new_charged_arc(scope.heap_mut(), specifier.as_str(), src.as_str())?;
        let record = SourceTextModuleRecord::parse_source_with_vm(vm, scope.heap_mut(), source)?;
        let id = modules.add_module_with_specifier(specifier.as_str(), record)?;
        self.modules.insert(specifier.clone(), id);
        id
      }
    };

    vm.finish_loading_imported_module(
      scope,
      modules,
      self,
      referrer,
      module_request,
      payload,
      Ok(module_id),
    )
  }
}

#[test]
fn jsruntime_exec_module_basic_runs_module_code() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let promise = rt.exec_module("main.js", "globalThis.x = 1;")?;
  let mut scope = rt.heap.scope();
  scope.push_root(promise)?;

  let Value::Object(promise_obj) = promise else {
    panic!("JsRuntime::exec_module should return a Promise object");
  };
  assert_eq!(
    scope.heap().promise_state(promise_obj)?,
    PromiseState::Fulfilled
  );
  drop(scope);

  assert_eq!(rt.exec_script("globalThis.x")?, Value::Number(1.0));
  Ok(())
}

#[test]
fn jsruntime_exec_module_static_imports_resolve_via_host_loader() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let mut host = ();
  let mut hooks = TestHostHooks::new();

  hooks.add_module_source("./dep.js", "export const x = 41;");

  let promise = rt.exec_module_with_host_and_hooks(
    &mut host,
    &mut hooks,
    "main.js",
    r#"
      import { x } from "./dep.js";
      globalThis.y = x + 1;
    "#,
  )?;

  let mut scope = rt.heap.scope();
  scope.push_root(promise)?;
  let Value::Object(promise_obj) = promise else {
    panic!("JsRuntime::exec_module_with_host_and_hooks should return a Promise object");
  };
  assert_eq!(
    scope.heap().promise_state(promise_obj)?,
    PromiseState::Fulfilled
  );
  drop(scope);

  assert_eq!(hooks.load_calls.as_slice(), ["./dep.js"]);
  assert_eq!(
    rt.exec_script_with_host_and_hooks(&mut host, &mut hooks, "globalThis.y")?,
    Value::Number(42.0)
  );
  Ok(())
}

#[test]
fn jsruntime_exec_module_dynamic_import_works_end_to_end() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let mut host = ();
  let mut hooks = TestHostHooks::new();

  hooks.add_module_source("./dep.js", "export const x = 42;");

  let promise = rt.exec_module_with_host_and_hooks(
    &mut host,
    &mut hooks,
    "main.js",
    r#"
      import("./dep.js").then((m) => { globalThis.v = m.x; });
    "#,
  )?;

  let mut scope = rt.heap.scope();
  scope.push_root(promise)?;
  let Value::Object(promise_obj) = promise else {
    panic!("JsRuntime::exec_module_with_host_and_hooks should return a Promise object");
  };
  assert_eq!(
    scope.heap().promise_state(promise_obj)?,
    PromiseState::Fulfilled
  );
  drop(scope);

  // Dynamic import settles asynchronously; module evaluation should have completed, but the `.then`
  // callback has not run yet.
  assert_eq!(
    rt.exec_script_with_host_and_hooks(&mut host, &mut hooks, "globalThis.v")?,
    Value::Undefined
  );

  hooks.perform_microtask_checkpoint(&mut rt, &mut host)?;

  assert_eq!(
    rt.exec_script_with_host_and_hooks(&mut host, &mut hooks, "globalThis.v")?,
    Value::Number(42.0)
  );
  Ok(())
}

#[test]
fn jsruntime_exec_module_hard_stop_tears_down_vm_microtasks() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Ensure there are no leftover jobs from runtime initialization.
  assert!(rt.vm.microtask_queue().is_empty());

  // Enqueue a Promise job, then spin until we exhaust fuel. If hard-stop teardown is broken, the
  // queued job will remain in the VM-owned microtask queue after `exec_module` returns the
  // termination error.
  rt.vm.set_budget(Budget {
    fuel: Some(200),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt
    .exec_module(
      "hard_stop.js",
      r#"
        Promise.resolve(1).then(() => 2);
        for (;;) {}
      "#,
    )
    .unwrap_err();

  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected termination, got {other:?}"),
  }

  assert!(
    rt.vm.microtask_queue().is_empty(),
    "hard-stop module exec should teardown queued microtasks for heap reuse"
  );

  // Reset budget so we can reuse the runtime.
  rt.vm.set_budget(Budget::unlimited(1));
  assert_eq!(rt.exec_script("1 + 1")?, Value::Number(2.0));
  Ok(())
}

#[test]
fn jsruntime_exec_module_top_level_await_is_async_and_requires_microtasks() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let promise = rt.exec_module(
    "tla.js",
    r#"
      await Promise.resolve();
      globalThis.done = 1;
    "#,
  )?;

  let Value::Object(promise_obj) = promise else {
    panic!("JsRuntime::exec_module should return a Promise object");
  };

  // The module evaluation promise must be pending until microtasks run.
  {
    let mut scope = rt.heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Pending);
  }

  // Drive microtasks until the evaluation promise settles.
  for _ in 0..8 {
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
    let mut scope = rt.heap.scope();
    scope.push_root(promise)?;
    if scope.heap().promise_state(promise_obj)? != PromiseState::Pending {
      break;
    }
  }

  let mut scope = rt.heap.scope();
  scope.push_root(promise)?;
  assert_eq!(
    scope.heap().promise_state(promise_obj)?,
    PromiseState::Fulfilled
  );
  drop(scope);

  assert_eq!(rt.exec_script("globalThis.done")?, Value::Number(1.0));
  Ok(())
}
