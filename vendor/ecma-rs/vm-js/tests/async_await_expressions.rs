use std::collections::HashMap;

use vm_js::{
  Heap, HeapLimits, HostDefined, JsRuntime, JsString, MicrotaskQueue, ModuleId, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, SourceTextModuleRecord, Value, Vm, VmError, VmHostHooks, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async/await and module loading tend to allocate more than simple synchronous scripts. Use a
  // slightly larger heap than the minimal 1MiB used by some unit tests to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
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
fn await_in_destructuring_assignment_default() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let x;
        ({ a: x = await Promise.resolve("ok") } = {});
        return x;
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
fn await_in_destructuring_assignment_computed_key_and_result() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let x;
        let obj = { k: "v" };
        let res = ({ [await Promise.resolve("k")]: x } = obj);
        return res.k + x;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "vv");
  Ok(())
}

#[test]
fn await_in_new_expression() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        function C(x) { this.x = x; }
        return new C(await Promise.resolve("ok")).x;
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
fn await_in_new_parenthesized_call_evaluates_expression_first() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        function make(x) { return function C(){ this.x = x; }; }
        // `new (make(await ...))` must evaluate `make(await ...)` first (producing a constructor),
        // then construct the *result* with no arguments. This differs from `new make(await ...)`.
        return (new (make(await Promise.resolve("ok")))).x;
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
fn await_in_delete_expression() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let obj = { k: 1 };
        delete obj[await Promise.resolve("k")];
        return String("k" in obj);
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "false");
  Ok(())
}

#[test]
fn await_in_delete_expression_strict_mode_nonconfigurable_member_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f(obj) {
        "use strict";
        try {
          delete (await Promise.resolve(obj)).x;
          return "no";
        } catch (e) {
          return e.name;
        }
      }
      var obj = {};
      Object.defineProperty(obj, "x", { value: 1, configurable: false });
      f(obj).then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "TypeError");
  Ok(())
}

#[test]
fn await_in_delete_expression_strict_mode_nonconfigurable_computed_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f(obj) {
        "use strict";
        try {
          delete obj[await Promise.resolve("x")];
          return "no";
        } catch (e) {
          return e.name;
        }
      }
      var obj = {};
      Object.defineProperty(obj, "x", { value: 1, configurable: false });
      f(obj).then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "TypeError");
  Ok(())
}

#[test]
fn await_in_delete_expression_optional_chain_skips_key() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        var hit = "no";
        var ok = delete (await Promise.resolve(null))?.[
          (hit = "yes", await Promise.resolve("k"))
        ];
        return String(ok) + hit;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "trueno");
  Ok(())
}

#[test]
fn await_in_class_decl_computed_method_name() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class C {
          [(await Promise.resolve("m"))]() { return "ok"; }
        }
        return new C().m();
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
fn await_in_named_class_expr_computed_key_can_reference_class_binding() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let C = class D {
          [(await Promise.resolve(D.name))]() { return "ok"; }
        };
        return new C().D();
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
  modules: HashMap<JsString, ModuleId>,
}

impl SyncHostHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      modules: HashMap::new(),
    }
  }

  fn register_module(&mut self, specifier: &str, module: ModuleId) {
    self
      .modules
      .insert(JsString::from_str(specifier).unwrap(), module);
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
      .get(&module_request.specifier)
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
  // Dynamic import allocates module graph state and promise capabilities before the first microtask
  // checkpoint. Keep the heap small to catch leaks, but large enough to cover the import pipeline.
  let mut rt = new_runtime();

  let record = SourceTextModuleRecord::parse(&mut rt.heap, "export const x = 'ok';")?;
  let m = rt.modules_mut().add_module(record)?;

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
