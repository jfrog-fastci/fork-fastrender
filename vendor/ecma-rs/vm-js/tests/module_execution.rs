use vm_js::{
  Heap, HeapLimits, ImportMetaProperty, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey, Realm, Scope,
  SourceText, SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};
use std::any::Any;

fn new_vm_heap_realm() -> Result<(Vm, Heap, Realm), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  // Module evaluation/linking tests exercise fairly allocation-heavy paths (parsing, instantiation,
  // namespace creation). Keep the heap small to catch leaks, but not so small that minor engine
  // changes trip spurious OOM failures.
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
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
  scope.get_with_host_and_hooks(vm, host, hooks, ns, key, Value::Object(ns))
}

#[test]
fn module_evaluate_supports_named_default_imports_and_live_bindings() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let a = graph.add_module_with_specifier(
    "a.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export let x = 1;
        export default 2;
        export function inc() { x = x + 1; }
      "#,
    )?,
  )?;
  let b = graph.add_module_with_specifier(
    "b.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import y, { x, inc } from "a.js";
        export const before = x;
        inc();
        export const after = x;
        export const sum = x + y;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

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
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

  let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "before")?,
    Value::Number(1.0)
  );
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "after")?,
    Value::Number(2.0)
  );
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "sum")?,
    Value::Number(4.0)
  );

  // Live export bindings: `inc()` mutated `a.x`, and the updated value must be visible through both
  // the importer and the exporting module namespace.
  let ns_a = graph.get_module_namespace(a, &mut vm, &mut scope)?;
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_a, "x")?,
    Value::Number(2.0)
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_module_instantiation_supports_anonymous_default_export_function() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  // `export default function() {}` is lowered by `hir-js` with a synthetic `"<anonymous>"` name.
  // Module instantiation must initialize the engine-internal `*default*` binding (created by module
  // linking) rather than creating a binding for `"<anonymous>"`.
  let src_a = "export default function() { return 123; }";
  let src_b = "import f from 'a.js'; export const v = f(); export const n = f.name;";

  let mut graph = ModuleGraph::new();

  // Module A: drop its retained AST and ensure linking/instantiation can proceed using only
  // compiled HIR.
  let src_a = SourceText::new_charged_arc(&mut heap, "a.js", src_a)?;
  let mut rec_a = SourceTextModuleRecord::compile_source(&mut heap, src_a)?;
  rec_a.ast = None;
  let a = graph.add_module_with_specifier("a.js", rec_a)?;

  // Module B can remain AST-backed; it exercises the default import binding.
  let b = graph.add_module_with_specifier("b.js", SourceTextModuleRecord::parse(&mut heap, src_b)?)?;

  graph.link_all_by_specifier();

  // Link first (compiled HIR instantiation runs here), then restore the AST for evaluation.
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
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn module_evaluate_sync_rejects_top_level_await_in_dependencies() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "dep.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        globalThis.dep_executed = true;
        export const v = await Promise.resolve(1);
      "#,
    )?,
  )?;
  let main = graph.add_module_with_specifier(
    "main.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import { v } from "dep.js";
        export const out = v + 1;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  let err = graph
    .evaluate_sync(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      main,
      &mut host,
      &mut hooks,
    )
    .expect_err("sync module evaluation should reject graphs that contain top-level await");
  let mut scope = heap.scope();
  match err {
    // Host-facing APIs coerce internal throw-completion errors (including `Unimplemented`) into a
    // JavaScript `Error` instance when intrinsics exist.
    VmError::ThrowWithStack { value, .. } | VmError::Throw(value) => {
      let Value::Object(err_obj) = value else {
        return Err(VmError::InvariantViolation(
          "expected sync module evaluation failure to throw an Error object",
        ));
      };
      scope.push_root(Value::Object(err_obj))?;
      let Value::String(message) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, err_obj, "message")?
      else {
        return Err(VmError::InvariantViolation(
          "expected sync module evaluation rejection Error to have a string `message` property",
        ));
      };
      let msg = scope.heap().get_string(message)?.to_utf8_lossy();
      assert!(
        msg.contains("top-level await"),
        "error message should mention top-level await (got {msg:?})"
      );
    }
    // Best-effort fallback when no intrinsics exist.
    VmError::Unimplemented(msg) => assert!(
      msg.contains("top-level await"),
      "error message should mention top-level await (got {msg:?})"
    ),
    other => panic!("unexpected module evaluation failure: {other:?}"),
  }

  // Ensure we failed fast (no module bodies executed).
  let key_s = scope.alloc_string("dep_executed")?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  assert_eq!(
    scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key)?,
    None,
    "dependency module body should not have executed"
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn module_evaluate_supports_anonymous_default_export_function_decls() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "a",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export default function() { return 1; }
      "#,
    )?,
  )?;
  let b = graph.add_module_with_specifier(
    "b",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import f from "a";
        export const r = f();
        export const n = f.name;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

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
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn module_evaluate_supports_reexports_and_export_star_as_namespace() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "base.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export const foo = 10;
        export const bar = 5;
        export default 1;
      "#,
    )?,
  )?;
  let reexport = graph.add_module_with_specifier(
    "reexport.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export { foo as f } from "base.js";
        export * from "base.js";
        export * as ns from "base.js";
      "#,
    )?,
  )?;
  let consumer = graph.add_module_with_specifier(
    "consumer.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import { foo, bar, f, ns } from "reexport.js";
        export const sum = foo + bar + f + ns.foo;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  let promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    consumer,
    &mut host,
    &mut hooks,
  )?;

  let mut scope = heap.scope();
  scope.push_root(promise)?;
  let Value::Object(promise_obj) = promise else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

  let ns_consumer = graph.get_module_namespace(consumer, &mut vm, &mut scope)?;
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "sum")?,
    Value::Number(35.0)
  );

  // `export * from` does not re-export `default`; ensure the namespace export list reflects that.
  graph.get_module_namespace(reexport, &mut vm, &mut scope)?;
  assert_eq!(
    graph.module_namespace_exports(reexport).unwrap(),
    &[
      "bar".to_string(),
      "f".to_string(),
      "foo".to_string(),
      "ns".to_string()
    ]
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn module_evaluate_handles_cycles_with_function_decls() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let a = graph.add_module_with_specifier(
    "a.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import { getB, fromA } from "b.js";
        export function getA() { return "A"; }
        export const fromB = getB();
        export const alsoFromA = fromA;
      "#,
    )?,
  )?;
  let b = graph.add_module_with_specifier(
    "b.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import { getA } from "a.js";
        export function getB() { return "B" + getA(); }
        export const fromA = getA();
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  let promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    a,
    &mut host,
    &mut hooks,
  )?;

  let mut scope = heap.scope();
  scope.push_root(promise)?;
  let Value::Object(promise_obj) = promise else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };
  let state = scope.heap().promise_state(promise_obj)?;
  if state != PromiseState::Fulfilled {
    let reason = scope.heap().promise_result(promise_obj)?.unwrap_or(Value::Undefined);
    let mut extra = String::new();
    if let Value::Object(err_obj) = reason {
      let mut msg_scope = scope.reborrow();
      msg_scope.push_root(Value::Object(err_obj))?;
      let k_name = PropertyKey::from_string(msg_scope.alloc_string("name")?);
      msg_scope.push_root(match k_name {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      })?;
      let k_message = PropertyKey::from_string(msg_scope.alloc_string("message")?);
      msg_scope.push_root(match k_message {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      })?;

      let name = msg_scope
        .heap()
        .object_get_own_data_property_value(err_obj, &k_name)?
        .and_then(|v| match v {
          Value::String(s) => Some(msg_scope.heap().get_string(s).ok()?.to_utf8_lossy()),
          _ => None,
        });
      let message = msg_scope
        .heap()
        .object_get_own_data_property_value(err_obj, &k_message)?
        .and_then(|v| match v {
          Value::String(s) => Some(msg_scope.heap().get_string(s).ok()?.to_utf8_lossy()),
          _ => None,
        });

      if let (Some(name), Some(message)) = (name, message) {
        extra = format!(" ({name}: {message})");
      }
    }
    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    realm.teardown(&mut heap);
    panic!("expected cycle evaluation to fulfill, got {state:?} with {reason:?}{extra}");
  }

  let ns_a = graph.get_module_namespace(a, &mut vm, &mut scope)?;
  let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;

  let Value::String(from_b) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_a, "fromB")? else {
    panic!("expected a.fromB to be a string");
  };
  assert_eq!(scope.heap().get_string(from_b)?.to_utf8_lossy(), "BA");

  let Value::String(from_a) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "fromA")? else {
    panic!("expected b.fromA to be a string");
  };
  assert_eq!(scope.heap().get_string(from_a)?.to_utf8_lossy(), "A");

  let Value::String(also_from_a) =
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_a, "alsoFromA")?
  else {
    panic!("expected a.alsoFromA to be a string");
  };
  assert_eq!(scope.heap().get_string(also_from_a)?.to_utf8_lossy(), "A");

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[derive(Default)]
struct ImportMetaHooks {
  queue: MicrotaskQueue,
  url: String,
  get_calls: u32,
  finalize_calls: u32,
}

impl VmHostHooks for ImportMetaHooks {
  fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
    self.queue.host_enqueue_promise_job(job, realm);
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
    self.get_calls += 1;

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
    self.finalize_calls += 1;
    Ok(())
  }
}

#[test]
fn module_evaluate_supports_import_meta_and_caches_it_per_module() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = ImportMetaHooks {
    url: "https://example.invalid/module.js".to_string(),
    ..Default::default()
  };
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let module = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export const meta1 = import.meta;
        export const meta2 = import.meta;
        export const url = import.meta.url;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  let promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    module,
    &mut host,
    &mut hooks,
  )?;

  let mut scope = heap.scope();
  scope.push_root(promise)?;
  let Value::Object(promise_obj) = promise else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

  let ns = graph.get_module_namespace(module, &mut vm, &mut scope)?;

  let Value::Object(meta1) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "meta1")? else {
    panic!("expected meta1 to be an object");
  };
  let Value::Object(meta2) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "meta2")? else {
    panic!("expected meta2 to be an object");
  };
  assert_eq!(meta1, meta2, "import.meta should be cached per module");
  assert_eq!(scope.heap().object_prototype(meta1)?, None);

  let Value::String(url) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "url")? else {
    panic!("expected url export to be a string");
  };
  assert_eq!(
    scope.heap().get_string(url)?.to_utf8_lossy(),
    "https://example.invalid/module.js"
  );

  assert_eq!(hooks.get_calls, 1);
  assert_eq!(hooks.finalize_calls, 1);

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn import_meta_is_scoped_to_defining_module_across_function_calls() -> Result<(), VmError> {
  // `import.meta` is evaluated using `GetActiveScriptOrModule()`. When calling a function imported
  // from another module, that function's `[[ScriptOrModule]]` must be observed (not the caller
  // module).
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "b.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export const meta = import.meta;
        export function getMeta() { return import.meta; }
      "#,
    )?,
  )?;
  let a = graph.add_module_with_specifier(
    "a.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import { meta as metaB, getMeta } from "b.js";
        export const metaA = import.meta;
        export const metaB_from_import = metaB;
        export const metaB_from_call = getMeta();
         export const import_binding_distinct = metaA !== metaB;
         export const call_distinct = metaA !== metaB_from_call;
         export const import_equals_call = metaB === metaB_from_call;
       "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  let promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    a,
    &mut host,
    &mut hooks,
  )?;

  let mut scope = heap.scope();
  scope.push_root(promise)?;
  let Value::Object(promise_obj) = promise else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

  let ns = graph.get_module_namespace(a, &mut vm, &mut scope)?;

  let Value::Object(meta_a) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "metaA")? else {
    panic!("expected metaA to be an object");
  };
  let Value::Object(meta_b_import) =
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "metaB_from_import")?
  else {
    panic!("expected metaB_from_import to be an object");
  };
  let Value::Object(meta_b_call) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "metaB_from_call")?
  else {
    panic!("expected metaB_from_call to be an object");
  };

  assert_ne!(
    meta_a, meta_b_import,
    "import.meta objects must be distinct across modules"
  );
  assert_eq!(
    meta_b_import, meta_b_call,
    "import.meta accessed via import binding should match the one returned from the exporting module's function"
  );

  assert_eq!(
    ns_get(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      ns,
      "import_binding_distinct"
    )?,
    Value::Bool(true),
  );
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "call_distinct")?,
    Value::Bool(true),
  );
  assert_eq!(
    ns_get(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      ns,
      "import_equals_call"
    )?,
    Value::Bool(true),
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn import_meta_rejects_escape_sequences_in_import_keyword() -> Result<(), VmError> {
  // test262: language/expressions/import.meta/syntax/escape-sequence-import.js
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let err = SourceTextModuleRecord::parse(&mut heap, r"im\u0070ort.meta;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
  Ok(())
}
