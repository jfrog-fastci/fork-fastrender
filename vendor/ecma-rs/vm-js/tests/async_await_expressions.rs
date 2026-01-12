use std::collections::HashMap;

use vm_js::{
  Heap, HeapLimits, HostDefined, JsRuntime, MicrotaskQueue, ModuleId, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, SourceTextModuleRecord, Value, Vm, VmError, VmHostHooks, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn await_in_array_literal() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        return [await Promise.resolve("a"), "b"].join("");
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ab");
  Ok(())
}

#[test]
fn await_in_object_literal_property_value() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        return ({ x: await Promise.resolve("ok") }).x;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_in_template_literal() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        return `a${await Promise.resolve("b")}c`;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "abc");
  Ok(())
}

#[test]
fn await_in_assignment_expression() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let x;
        x = await Promise.resolve(1);
        return x;
      }
      f().then(function (v) { out = String(v); });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "1");
  Ok(())
}

#[test]
fn await_in_comma_expression() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        return (0, await Promise.resolve("ok"));
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

/// Host hook implementation that completes `HostLoadImportedModule` synchronously by immediately
/// calling `FinishLoadingImportedModule`, plus an owned microtask queue for async/await and Promise
/// jobs.
struct SyncHostHooks {
  microtasks: MicrotaskQueue,
  modules: HashMap<String, ModuleId>,
}

impl SyncHostHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      modules: HashMap::new(),
    }
  }

  fn register_module(&mut self, specifier: &str, module: ModuleId) {
    self.modules.insert(specifier.to_string(), module);
  }

  fn teardown_jobs(&mut self, rt: &mut JsRuntime) {
    self.microtasks.teardown(rt);
  }

  fn perform_microtask_checkpoint(&mut self, rt: &mut JsRuntime) -> Result<(), VmError> {
    if !self.microtasks.begin_checkpoint() {
      return Ok(());
    }

    let mut errors = Vec::new();
    while let Some((_realm, job)) = self.microtasks.pop_front() {
      if let Err(err) = job.run(rt, self) {
        let is_termination = matches!(err, VmError::Termination(_));
        errors.push(err);
        if is_termination {
          self.microtasks.teardown(rt);
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
}

impl VmHostHooks for SyncHostHooks {
  fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
    self.microtasks.host_enqueue_promise_job(job, realm);
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    &[]
  }

  fn host_load_imported_module(
    &mut self,
    vm: &mut Vm,
    scope: &mut vm_js::Scope<'_>,
    modules: &mut vm_js::ModuleGraph,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    _host_defined: HostDefined,
    payload: ModuleLoadPayload,
  ) -> Result<(), VmError> {
    let module = *self
      .modules
      .get(module_request.specifier.as_str())
      .unwrap_or_else(|| {
        panic!(
          "no module registered for specifier {:?}",
          module_request.specifier
        )
      });
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
fn await_in_import_specifier() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let m = rt.modules_mut().add_module(SourceTextModuleRecord::parse(
    "export const x = 'ok';",
  )?);

  let mut host = SyncHostHooks::new();
  host.register_module("./m.js", m);

  let value = rt.exec_script_with_hooks(
    &mut host,
    r#"
      var out = "";
      async function f() {
        const ns = await import(await Promise.resolve("./m.js"));
        return ns.x;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  host.perform_microtask_checkpoint(&mut rt)?;

  let value = rt.exec_script_with_hooks(&mut host, "out")?;
  assert_eq!(value_to_string(&rt, value), "ok");

  host.teardown_jobs(&mut rt);
  Ok(())
}

