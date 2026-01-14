use std::collections::HashMap;

use vm_js::{
  CompiledScript, Heap, HeapLimits, HostDefined, JsString, MicrotaskQueue, ModuleGraph, ModuleId, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, PromiseState, PropertyKey, Realm, Scope, SourceTextModuleRecord, Value, Vm, VmError,
  VmHost, VmHostHooks, VmJobContext, VmOptions,
};

mod _async_generator_support;

fn new_vm_heap_realm() -> Result<(Vm, Heap, Realm), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  // Keep this moderately small to catch leaks, but large enough for module parsing + namespace creation.
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let realm = Realm::new(&mut vm, &mut heap)?;
  Ok((vm, heap, realm))
}

fn compile_module_record_without_ast(
  heap: &mut Heap,
  name: &str,
  source: &str,
) -> Result<SourceTextModuleRecord, VmError> {
  let script = CompiledScript::compile_module(heap, name, source)?;
  assert!(
    !script.requires_ast_fallback,
    "test module should compile without requiring full AST fallback"
  );
  let mut record = SourceTextModuleRecord::parse_source(script.source.clone())?;
  record.compiled = Some(script);
  // Force `ModuleGraph` to instantiate/evaluate via the compiled module path.
  record.ast = None;
  Ok(record)
}

fn get_global_data_property(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  global: vm_js::GcObject,
  name: &str,
) -> Result<Value, VmError> {
  scope.push_root(Value::Object(global))?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.get_with_host_and_hooks(vm, host, hooks, global, key, Value::Object(global))
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

fn promise_rejection_message_contains(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  promise: vm_js::GcObject,
  needle: &str,
) -> Result<bool, VmError> {
  if scope.heap().promise_state(promise)? != PromiseState::Rejected {
    return Ok(false);
  }
  let Some(reason) = scope.heap().promise_result(promise)? else {
    return Ok(false);
  };

  match reason {
    Value::String(s) => Ok(scope.heap().get_string(s)?.to_utf8_lossy().contains(needle)),
    Value::Object(obj) => {
      // Internal module-graph errors are converted to a stable `Error` instance; probe its `.message`.
      let Value::String(message) = ns_get(vm, host, hooks, scope, obj, "message")? else {
        return Ok(false);
      };
      Ok(scope.heap().get_string(message)?.to_utf8_lossy().contains(needle))
    }
    _ => Ok(false),
  }
}

struct MicrotaskCtx<'a> {
  vm: &'a mut Vm,
  heap: &'a mut Heap,
  host: &'a mut dyn VmHost,
}

impl VmJobContext for MicrotaskCtx<'_> {
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

fn run_microtasks(
  vm: &mut Vm,
  heap: &mut Heap,
  host: &mut dyn VmHost,
  hooks: &mut MicrotaskQueue,
) -> Result<(), VmError> {
  let mut ctx = MicrotaskCtx { vm, heap, host };
  let errors = hooks.perform_microtask_checkpoint(&mut ctx);
  if errors.is_empty() {
    Ok(())
  } else {
    Err(errors
      .into_iter()
      .next()
      .unwrap_or_else(|| VmError::InvariantViolation("microtask checkpoint returned empty errors")))
  }
}

fn supports_compiled_modules(vm: &mut Vm, heap: &mut Heap, realm: &Realm) -> bool {
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut graph = ModuleGraph::new();

  let supported = (|| {
    let script = match CompiledScript::compile_module(heap, "feature_probe.js", "export default 1;") {
      Ok(s) => s,
      // If compilation fails for a valid module, treat it as "supported" so tests fail rather than
      // silently skipping.
      Err(_) => return true,
    };
    let mut record = match SourceTextModuleRecord::parse_source(script.source.clone()) {
      Ok(r) => r,
      Err(_) => return true,
    };
    record.compiled = Some(script);
    record.ast = None;

    if graph.add_module_with_specifier("a", record).is_err() {
      return true;
    };
    let Ok(record_b) = SourceTextModuleRecord::parse(heap, "import x from \"a\"; export const y = x;") else {
      return true;
    };
    let Ok(b) = graph.add_module_with_specifier("b", record_b) else {
      return true;
    };
    graph.link_all_by_specifier();

    let promise = match graph.evaluate(
      vm,
      heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return false,
      // Any other error means the compiled-module path exists but may be buggy; don't skip.
      Err(_) => return true,
    };

    let mut scope = heap.scope();
    if scope.push_root(promise).is_err() {
      return true;
    }
    let Value::Object(promise_obj) = promise else {
      return true;
    };

    // If ModuleGraph rejects the promise with an internal "module AST missing" error, compiled
    // modules aren't supported yet.
    match promise_rejection_message_contains(
      vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    ) {
      Ok(true) => false,
      _ => true,
    }
  })();

  graph.teardown(vm, heap);
  // Discard any jobs so dropping `hooks` doesn't trip `Job` root-leak debug assertions.
  let mut ctx = MicrotaskCtx { vm, heap, host: &mut host };
  hooks.teardown(&mut ctx);
  supported
}

fn run_compiled_module_local_import_case(consumer_compiled: bool) -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let a = graph.add_module_with_specifier(
      "a",
      compile_module_record_without_ast(&mut heap, "a.js", r#"export const x = 1;"#)?,
    )?;

    let record_b_src = r#"import { x } from "a"; export const y = x + 1;"#;
    let b_record = if consumer_compiled {
      compile_module_record_without_ast(&mut heap, "b.js", record_b_src)?
    } else {
      SourceTextModuleRecord::parse(&mut heap, record_b_src)?
    };
    let b = graph.add_module_with_specifier("b", b_record)?;
    graph.link_all_by_specifier();

    let mut scope = heap.scope();
    graph.evaluate_sync_with_scope(
      &mut vm,
      &mut scope,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    )?;

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "y")?,
      Value::Number(2.0)
    );

    // Avoid unused variable warnings for `a`.
    let _ = a;
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);
  result
}

#[test]
fn compiled_module_graph_local_exports_import_ast_consumer() -> Result<(), VmError> {
  run_compiled_module_local_import_case(false)
}

#[test]
fn compiled_module_graph_local_exports_import_compiled_consumer() -> Result<(), VmError> {
  run_compiled_module_local_import_case(true)
}

#[test]
fn compiled_module_graph_default_export_expression_is_evaluated_once() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    graph.add_module_with_specifier(
      "a",
      compile_module_record_without_ast(
        &mut heap,
        "a.js",
        r#"export default (globalThis.__seen = (globalThis.__seen||0)+1, 10); export const z = 1;"#,
      )?,
    )?;

    let b = graph.add_module_with_specifier(
      "b",
      compile_module_record_without_ast(
        &mut heap,
        "b.js",
        r#"import d, { z } from "a"; export const out = d + z;"#,
      )?,
    )?;
    graph.link_all_by_specifier();

    let mut scope = heap.scope();
    graph.evaluate_sync_with_scope(
      &mut vm,
      &mut scope,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    )?;

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "out")?,
      Value::Number(11.0)
    );

    let seen = get_global_data_property(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      realm.global_object(),
      "__seen",
    )?;
    assert_eq!(seen, Value::Number(1.0));

    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);
  result
}

struct SyncDynamicImportHooks {
  microtasks: MicrotaskQueue,
  modules: HashMap<JsString, ModuleId>,
  seen_referrer: Option<ModuleReferrer>,
}

impl SyncDynamicImportHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      modules: HashMap::new(),
      seen_referrer: None,
    }
  }

  fn register_module(&mut self, specifier: &str, module: ModuleId) {
    self
      .modules
      .insert(JsString::from_str(specifier).unwrap(), module);
  }

  fn perform_microtask_checkpoint(&mut self, ctx: &mut dyn VmJobContext) -> Result<(), VmError> {
    if !self.microtasks.begin_checkpoint() {
      return Ok(());
    }

    let mut errors = Vec::new();
    while let Some((_realm, job)) = self.microtasks.pop_front() {
      if let Err(err) = job.run(ctx, self) {
        let is_hard_stop = matches!(err, VmError::Termination(_) | VmError::OutOfMemory);
        errors.push(err);
        if is_hard_stop {
          self.microtasks.teardown(ctx);
          break;
        }
      }
    }
    self.microtasks.end_checkpoint();

    if let Some(err) = errors.into_iter().next() {
      Err(err)
    } else {
      Ok(())
    }
  }

  fn teardown_jobs(&mut self, ctx: &mut dyn VmJobContext) {
    self.microtasks.teardown(ctx);
  }
}

impl VmHostHooks for SyncDynamicImportHooks {
  fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
    self.microtasks.host_enqueue_promise_job(job, realm);
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    &[]
  }

  fn host_call_job_callback(
    &mut self,
    ctx: &mut dyn VmJobContext,
    callback: &vm_js::JobCallback,
    this_argument: Value,
    arguments: &[Value],
  ) -> Result<Value, VmError> {
    ctx.call(
      self,
      Value::Object(callback.callback_object()),
      this_argument,
      arguments,
    )
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
    self.seen_referrer = Some(referrer);
    let module = *self.modules.get(&module_request.specifier).unwrap_or_else(|| {
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

  fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
    Some(self)
  }
}

#[test]
fn compiled_module_graph_dynamic_import_from_compiled_module_resolves() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut host = ();
  let mut hooks = SyncDynamicImportHooks::new();
  let mut graph = ModuleGraph::new();

  // Make the graph available for dynamic `import()` executed after module evaluation.
  vm.set_module_graph(&mut graph);

  let result = (|| -> Result<(), VmError> {
    let dep = graph.add_module_with_specifier(
      "dep",
      SourceTextModuleRecord::parse(&mut heap, r#"export const v = 42;"#)?,
    )?;
    hooks.register_module("dep", dep);

    let entry = graph.add_module_with_specifier(
      "entry",
      compile_module_record_without_ast(
        &mut heap,
        "entry.js",
        r#"export async function f(){ return import("dep").then(m => m.v); }"#,
      )?,
    )?;

    graph.link_all_by_specifier();

    let mut scope = heap.scope();
    graph.evaluate_sync_with_scope(
      &mut vm,
      &mut scope,
      realm.global_object(),
      realm.id(),
      entry,
      &mut host,
      &mut hooks,
    )?;

    let ns_entry = graph.get_module_namespace(entry, &mut vm, &mut scope)?;
    let f = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_entry, "f")?;
    let promise = vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, f, Value::Undefined, &[])?;
    let promise_root = scope.heap_mut().add_root(promise)?;

    drop(scope);

    let mut ctx = MicrotaskCtx {
      vm: &mut vm,
      heap: &mut heap,
      host: &mut host,
    };
    hooks.perform_microtask_checkpoint(&mut ctx)?;

    let Some(Value::Object(promise_obj)) = heap.get_root(promise_root) else {
      panic!("expected call to async function to return a Promise object");
    };
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    assert_eq!(heap.promise_result(promise_obj)?, Some(Value::Number(42.0)));
    heap.remove_root(promise_root);

    assert_eq!(hooks.seen_referrer, Some(ModuleReferrer::Module(entry)));
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown_jobs(&mut ctx);
  realm.teardown(&mut heap);
  result
}

#[test]
fn compiled_module_graph_import_meta_is_cached_within_compiled_module() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();
  let mut graph = ModuleGraph::new();

  // `import.meta` requires a module graph even after evaluation (when calling exported functions).
  vm.set_module_graph(&mut graph);

  let result = (|| -> Result<(), VmError> {
    let m = graph.add_module_with_specifier(
      "m",
      compile_module_record_without_ast(
        &mut heap,
        "m.js",
        r#"export function getMeta(){ return import.meta; }"#,
      )?,
    )?;
    graph.link_all_by_specifier();

    let mut scope = heap.scope();
    graph.evaluate_sync_with_scope(
      &mut vm,
      &mut scope,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;

    let ns_m = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    let get_meta = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_m, "getMeta")?;
    let v1 = vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, get_meta, Value::Undefined, &[])?;
    let v2 = vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, get_meta, Value::Undefined, &[])?;
    let (Value::Object(m1), Value::Object(m2)) = (v1, v2) else {
      panic!("expected getMeta() to return an object");
    };
    assert_eq!(m1, m2);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);
  result
}

#[test]
fn compiled_module_supports_anonymous_default_export_function_decls() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    // Create a cyclic graph so the importer module executes before the exporting module. This
    // requires the default-exported function binding to be initialized during module instantiation
    // (hoisted), not during module evaluation.
    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        import { fromB } from "b";
        export default function() { return 1; }
        export const seenFromB = fromB;
      "#,
    )?;
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import f from "a";
          export const fromB = f();
          export const n = f.name;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), a) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      a,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "fromB")?,
      Value::Number(1.0)
    );
    let Value::String(n) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "n")? else {
      panic!("expected b.n to be a string");
    };
    assert_eq!(scope.heap().get_string(n)?.to_utf8_lossy(), "default");

    drop(scope);

    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_anonymous_default_export_class_decl_is_tdz_in_cycles() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    // Create a cyclic graph so the importer module executes before the exporting module.
    // Unlike function declarations, class declarations are *not* hoisted/initialized during module
    // instantiation; accessing the default import before the declaration executes must throw a TDZ
    // ReferenceError.
    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        import { touch } from "b";
        export default class {}
        export const seenTouch = touch;
      "#,
    )?;
    assert!(
      !script_a.requires_ast_fallback && !script_a.contains_async_generators,
      "expected module to be executable by compiled evaluator"
    );
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import C from "a";
          export const touch = 1;
          export const before = (() => {
            try { return C.name; } catch (e) { return e.message; }
          })();
          export function after() { return C.name; }
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), a) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      a,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    let Value::String(before) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "before")?
    else {
      panic!("expected b.before to be a string");
    };
    let before_s = scope.heap().get_string(before)?.to_utf8_lossy();
    assert!(
      before_s.contains("before initialization"),
      "unexpected TDZ error message: {before_s:?}"
    );

    let after = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "after")?;
    let after_res = vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, after, Value::Undefined, &[])?;
    let Value::String(name) = after_res else {
      panic!("expected b.after() to return a string");
    };
    assert_eq!(scope.heap().get_string(name)?.to_utf8_lossy(), "default");

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_anonymous_default_export_class_namespace_is_tdz_in_cycles() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    // Same cyclic setup as `compiled_module_anonymous_default_export_class_decl_is_tdz_in_cycles`,
    // but access the default export via the module namespace (`ns.default`).
    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        import { touch } from "b";
        export default class {}
        export const seenTouch = touch;
      "#,
    )?;
    assert!(
      !script_a.requires_ast_fallback && !script_a.contains_async_generators,
      "expected module to be executable by compiled evaluator"
    );
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import * as ns from "a";
          export const touch = 1;
          export const before = (() => {
            try { return ns.default.name; } catch (e) { return e.message; }
          })();
          export function after() { return ns.default.name; }
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), a) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      a,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    let Value::String(before) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "before")?
    else {
      panic!("expected b.before to be a string");
    };
    let before_s = scope.heap().get_string(before)?.to_utf8_lossy();
    assert!(
      before_s.contains("default") && before_s.contains("before initialization"),
      "unexpected TDZ error message: {before_s:?}"
    );

    let after = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "after")?;
    let after_res = vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, after, Value::Undefined, &[])?;
    let Value::String(name) = after_res else {
      panic!("expected b.after() to return a string");
    };
    assert_eq!(scope.heap().get_string(name)?.to_utf8_lossy(), "default");

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_class_field_initializer_direct_eval_allows_super() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        class Base {
          m() { return 1; }
          static sm() { return 2; }
        }
        export default class extends Base {
          x = eval("super.m()");
          static y = eval("super.sm()");
        }
      "#,
    )?;
    assert!(
      !script_a.requires_ast_fallback && !script_a.contains_async_generators,
      "expected module to be executable by compiled evaluator"
    );
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    // Drop the AST so module evaluation must use the compiled HIR path.
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import C from "a";
          export const inst = new C().x;
          export const stat = C.y;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "inst")?,
      Value::Number(1.0)
    );
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "stat")?,
      Value::Number(2.0)
    );

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_supports_anonymous_default_export_class_static_fields() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"export default class { static n = 1 }"#,
    )?;
    assert!(
      !script_a.requires_ast_fallback && !script_a.contains_async_generators,
      "expected module to be executable by compiled evaluator"
    );
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    // Drop the AST so module evaluation must use the compiled HIR path.
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import C from "a";
          export const out = C.n;
          export const name = C.name;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "out")?,
      Value::Number(1.0)
    );
    let Value::String(name) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "name")? else {
      panic!("expected b.name to be a string");
    };
    assert_eq!(scope.heap().get_string(name)?.to_utf8_lossy(), "default");

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_supports_anonymous_default_export_class_instance_fields() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"export default class { x = 1 }"#,
    )?;
    assert!(
      !script_a.requires_ast_fallback && !script_a.contains_async_generators,
      "expected module to be executable by compiled evaluator"
    );
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    // Drop the AST so module evaluation must use the compiled HIR path.
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import C from "a";
          export const out = new C().x;
          export const name = C.name;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "out")?,
      Value::Number(1.0)
    );
    let Value::String(name) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "name")? else {
      panic!("expected b.name to be a string");
    };
    assert_eq!(scope.heap().get_string(name)?.to_utf8_lossy(), "default");

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_supports_anonymous_default_export_class_static_blocks() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        class Base { static m() { return this; } }
        export default class extends Base {
          static { this.ok = super.m() === this; }
        }
      "#,
    )?;
    assert!(
      !script_a.requires_ast_fallback && !script_a.contains_async_generators,
      "expected module to be executable by compiled evaluator"
    );
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    // Drop the AST so module evaluation must use the compiled HIR path.
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import C from "a";
          export const ok = C.ok;
          export const name = C.name;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "ok")?,
      Value::Bool(true)
    );
    let Value::String(name) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "name")? else {
      panic!("expected b.name to be a string");
    };
    assert_eq!(scope.heap().get_string(name)?.to_utf8_lossy(), "default");

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_rejection_error_object_has_throw_site_stack() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script = CompiledScript::compile_module(
      &mut heap,
      "m.js",
      "const err = new Error('boom');\nthrow err;\nexport const unreachable = 1;",
    )?;
    let mut record = SourceTextModuleRecord::parse_source(script.source.clone())?;
    record.compiled = Some(script);
    // Drop the AST so module evaluation must use the compiled HIR path.
    record.ast = None;
    let m = graph.add_module_with_specifier("m.js", record)?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), m) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(m).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }

    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Rejected);
    let reason = scope
      .heap()
      .promise_result(promise_obj)?
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

    // The `throw err;` statement is the 2nd line of the module and starts at column 1.
    let stack = scope.heap().get_string(stack_s)?.to_utf8_lossy();
    let first_frame = stack.lines().find(|line| line.starts_with("at ")).unwrap_or("");
    assert!(
      first_frame.starts_with("at m.js:2:1"),
      "unexpected stack trace: {stack:?}"
    );

    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_supports_anonymous_default_export_class_decls() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        export default class {
          constructor() { this.x = 1; }
        }
      "#,
    )?;
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import C from "a";
          export const r = new C().x;
          export const n = C.name;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "r")?,
      Value::Number(1.0)
    );
    let Value::String(n) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "n")? else {
      panic!("expected b.n to be a string");
    };
    assert_eq!(scope.heap().get_string(n)?.to_utf8_lossy(), "default");

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_export_default_expr_respects_statement_order() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        export const log = [];
        log.push("before");
        export default (log.push("default"), 123);
        log.push("after");
      "#,
    )?;
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import d, { log } from "a";
          export const out = log.join(",") + ":" + d;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    let Value::String(out) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "out")? else {
      panic!("expected b.out to be a string");
    };
    assert_eq!(
      scope.heap().get_string(out)?.to_utf8_lossy(),
      "before,default,after:123"
    );

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_export_default_expr_applies_set_function_name_default() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        export default (() => 123);
      "#,
    )?;
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import f from "a";
          export const v = f();
          export const n = f.name;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "v")?,
      Value::Number(123.0)
    );
    let Value::String(n) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "n")? else {
      panic!("expected b.n to be a string");
    };
    assert_eq!(scope.heap().get_string(n)?.to_utf8_lossy(), "default");

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_export_default_async_arrow_expr_applies_set_function_name_default(
) -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        export default (async () => (this === undefined ? 123 : -100));
      "#,
    )?;
    assert!(
      script_a.contains_async_functions,
      "test module should contain at least one async function"
    );
    assert!(
      !script_a.requires_ast_fallback && !script_a.contains_async_generators,
      "modules that only *define* async functions should be executable via the compiled evaluator"
    );

    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import f from "a";
          export let r = 0;
          export const n = f.name;
          f.call({}).then(v => { r = v; });
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b)?;
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    )?;

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled,
      "module evaluation should complete synchronously"
    );
    drop(scope);

    run_microtasks(&mut vm, &mut heap, &mut host, &mut hooks)?;

    let mut scope = heap.scope();
    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "r")?,
      Value::Number(123.0)
    );
    let Value::String(n) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "n")? else {
      panic!("expected b.n to be a string");
    };
    assert_eq!(scope.heap().get_string(n)?.to_utf8_lossy(), "default");

    drop(scope);
    Ok(())
  })();

  // Always tear down persistent roots on all return paths (including skips / errors).
  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_export_default_async_function_expr_applies_set_function_name_default(
) -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        export default (async function () {
          return this === undefined ? 123 : -100;
        });
      "#,
    )?;
    assert!(
      script_a.contains_async_functions,
      "test module should contain at least one async function"
    );
    assert!(
      !script_a.requires_ast_fallback && !script_a.contains_async_generators,
      "modules that only *define* async functions should be executable via the compiled evaluator"
    );

    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import f from "a";
          export let r = 0;
          export const n = f.name;
          f().then(v => { r += v; });
          f.call({}).then(v => { r += v; });
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b)?;
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    )?;

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled,
      "module evaluation should complete synchronously"
    );
    drop(scope);

    run_microtasks(&mut vm, &mut heap, &mut host, &mut hooks)?;

    let mut scope = heap.scope();
    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "r")?,
      Value::Number(23.0)
    );
    let Value::String(n) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "n")? else {
      panic!("expected b.n to be a string");
    };
    assert_eq!(scope.heap().get_string(n)?.to_utf8_lossy(), "default");

    drop(scope);
    Ok(())
  })();

  // Always tear down persistent roots on all return paths (including skips / errors).
  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_export_default_class_expr_constructs_and_applies_set_function_name_default(
) -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        export default (class {});
      "#,
    )?;
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import C from "a";
          export const n = C.name;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    let Value::String(n) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "n")? else {
      panic!("expected b.n to be a string");
    };
    assert_eq!(scope.heap().get_string(n)?.to_utf8_lossy(), "default");

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_export_default_class_expr_static_name_method_can_override_and_internal_name_is_default(
) -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        export default (class { static name(){} });
      "#,
    )?;
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import C from "a";
          export const typeofName = typeof C.name;
          export const nameName = typeof C.name === "function" ? C.name.name : null;
          export const s = C.toString();
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    let Value::String(typeof_name) =
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "typeofName")?
    else {
      panic!("expected typeofName to be a string");
    };
    assert_eq!(scope.heap().get_string(typeof_name)?.to_utf8_lossy(), "function");

    let Value::String(name_name) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "nameName")?
    else {
      panic!("expected nameName to be a string");
    };
    assert_eq!(scope.heap().get_string(name_name)?.to_utf8_lossy(), "name");

    let Value::String(s) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "s")? else {
      panic!("expected s to be a string");
    };
    assert_eq!(
      scope.heap().get_string(s)?.to_utf8_lossy(),
      "function default() { [native code] }"
    );

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_export_default_function_expr_applies_set_function_name_default() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        export default (function() { return 123; });
      "#,
    )?;
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import f from "a";
          export const v = f();
          export const n = f.name;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "v")?,
      Value::Number(123.0)
    );
    let Value::String(n) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "n")? else {
      panic!("expected b.n to be a string");
    };
    assert_eq!(scope.heap().get_string(n)?.to_utf8_lossy(), "default");

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_export_default_class_expr_applies_set_function_name_default() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        export default (class { constructor() { this.x = 1; } });
      "#,
    )?;
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import C from "a";
          export const v = new C().x;
          export const n = C.name;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    match graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b) {
      Ok(()) => {}
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "v")?,
      Value::Number(1.0)
    );
    let Value::String(n) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "n")? else {
      panic!("expected b.n to be a string");
    };
    assert_eq!(scope.heap().get_string(n)?.to_utf8_lossy(), "default");

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_supports_anonymous_default_export_async_function_decls() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        export default async function() { return this === undefined ? 1 : -100; }
      "#,
    )?;
    assert!(
      script_a.contains_async_functions,
      "test module should contain at least one async function"
    );
    assert!(
      !script_a.requires_ast_fallback && !script_a.contains_async_generators,
      "modules that only *define* async functions should be executable via the compiled evaluator"
    );
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import f from "a";
          export let r = 0;
          export const n = f.name;
          f().then(v => { r = v; });
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b)?;
    assert!(
      graph.module(a).ast.is_none(),
      "linking should not parse/retain an AST when compiled HIR is available"
    );

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    )?;

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled,
      "module evaluation should complete synchronously"
    );
    drop(scope);

    run_microtasks(&mut vm, &mut heap, &mut host, &mut hooks)?;

    let mut scope = heap.scope();
    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "r")?,
      Value::Number(1.0)
    );
    let Value::String(n) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "n")? else {
      panic!("expected b.n to be a string");
    };
    assert_eq!(scope.heap().get_string(n)?.to_utf8_lossy(), "default");

    drop(scope);

    Ok(())
  })();

  // Always tear down persistent roots on all return paths (including skips / errors).
  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_supports_anonymous_default_export_generator_function_decls() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        export default function*() { yield 1; }
      "#,
    )?;
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import f from "a";
          export const r = f().next().value;
          export const n = f.name;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "r")?,
      Value::Number(1.0)
    );
    let Value::String(n) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "n")? else {
      panic!("expected b.n to be a string");
    };
    assert_eq!(scope.heap().get_string(n)?.to_utf8_lossy(), "default");

    drop(scope);

    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_hoists_top_level_function_decls_into_module_env() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        function f() { return 1; }
        export const x = f();
      "#,
    )?;
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import { x } from "a";
          export const y = x;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "y")?,
      Value::Number(1.0)
    );

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_supports_named_default_export_function_decls() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        export default function foo() { return 1; }
      "#,
    )?;
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import f from "a";
          export const v = f();
          export const n = f.name;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "v")?,
      Value::Number(1.0)
    );
    let Value::String(n) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "n")? else {
      panic!("expected b.n to be a string");
    };
    assert_eq!(scope.heap().get_string(n)?.to_utf8_lossy(), "foo");

    drop(scope);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}

#[test]
fn compiled_module_supports_anonymous_default_export_async_generator_function_decls() -> Result<(), VmError> {
  let mut rt = vm_js::JsRuntime::new(
    Vm::new(VmOptions::default()),
    Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024)),
  )?;
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }
  drop(rt);

  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let result = (|| -> Result<(), VmError> {
    if !supports_compiled_modules(&mut vm, &mut heap, &realm) {
      return Ok(());
    }

    let script_a = CompiledScript::compile_module(
      &mut heap,
      "a.js",
      r#"
        export default async function*() { yield 1; }
      "#,
    )?;
    let mut record_a = SourceTextModuleRecord::parse_source(script_a.source.clone())?;
    record_a.compiled = Some(script_a);
    record_a.ast = None;
    graph.add_module_with_specifier("a", record_a)?;

    let b = graph.add_module_with_specifier(
      "b",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import f from "a";
          export let r = 0;
          export const n = f.name;
          f().next().then(v => { r = v.value; });
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    let promise = match graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    if promise_rejection_message_contains(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      promise_obj,
      "module AST missing",
    )? {
      return Ok(());
    }
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
    drop(scope);

    run_microtasks(&mut vm, &mut heap, &mut host, &mut hooks)?;

    let mut scope = heap.scope();
    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "r")?,
      Value::Number(1.0)
    );
    let Value::String(n) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "n")? else {
      panic!("expected b.n to be a string");
    };
    assert_eq!(scope.heap().get_string(n)?.to_utf8_lossy(), "default");

    drop(scope);

    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  let mut ctx = MicrotaskCtx {
    vm: &mut vm,
    heap: &mut heap,
    host: &mut host,
  };
  hooks.teardown(&mut ctx);
  realm.teardown(&mut heap);

  result
}
