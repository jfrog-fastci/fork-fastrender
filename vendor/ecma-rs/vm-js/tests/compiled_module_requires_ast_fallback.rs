use vm_js::{
  Heap, HeapLimits, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey, Realm, Scope, SourceText,
  SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext, VmOptions,
};

fn new_vm_heap_realm() -> Result<(Vm, Heap, Realm), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  // Keep this moderately small to catch leaks, but large enough for module parsing + namespace creation.
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

#[test]
fn compiled_module_requires_ast_fallback_parses_ast_before_linking_and_evaluates() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    // Build a compiled module record via `SourceTextModuleRecord::compile_source` (module record +
    // compiled HIR).
    //
    // We deliberately include private names so the compiled-module execution path must fall back
    // to the AST interpreter (`CompiledScript::requires_ast_fallback`). This exercises the
    // ModuleGraph logic that parses/retains an AST on demand when only compiled HIR was retained.
    let src_a = SourceText::new_charged_arc(
      &mut heap,
      "a.js",
      r#"
        globalThis.__compiled_module_requires_ast_fallback_count =
          (globalThis.__compiled_module_requires_ast_fallback_count || 0) + 1;
        class C {
          #x = 1;
          getX() { return this.#x; }
        }
        export async function f() { return (new C()).getX(); }
      "#,
    )?;
    let mut rec_a = SourceTextModuleRecord::compile_source(&mut heap, src_a)?;
    assert!(
      rec_a
        .compiled
        .as_ref()
        .is_some_and(|c| c.requires_ast_fallback),
      "expected private-name module to require AST fallback"
    );
    // Drop both the retained AST and `record.source` so ModuleGraph must parse the AST on demand
    // from `compiled.source`.
    rec_a.ast = None;
    rec_a.source = None;

    let a = graph.add_module_with_specifier("a.js", rec_a)?;

    let b = graph.add_module_with_specifier(
      "b.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import { f } from "a.js";
          export const c = globalThis.__compiled_module_requires_ast_fallback_count;
          export let r = 0;
          f().then(v => { r = v; });
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    // `requires_ast_fallback` must be consulted *before* any compiled-module instantiation so the
    // module is instantiated via the interpreter path and does not partially run through the HIR
    // executor.
    graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b)?;
    assert!(
      graph.module(a).ast.is_some(),
      "expected ModuleGraph::link to parse/retain an AST for a compiled module that requires AST fallback"
    );
    assert!(
      graph.module(a).source.is_some(),
      "expected ModuleGraph::link to restore module source text when parsing an AST on demand"
    );

    // Evaluate the importer. Module execution should succeed (no `VmError::Unimplemented` from the
    // HIR executor).
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
    drop(scope);

    run_microtasks(&mut vm, &mut heap, &mut host, &mut hooks)?;

    let mut scope = heap.scope();
    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "c")?,
      Value::Number(1.0),
      "module should not be evaluated twice (no partial HIR execution before AST fallback)"
    );
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "r")?,
      Value::Number(1.0)
    );
    drop(scope);

    Ok(())
  })();

  // Always tear down persistent roots on all return paths.
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
fn compiled_module_requires_ast_fallback_is_respected_by_evaluate_sync() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    // Compiled module that requires AST fallback (private names).
    //
    // Drop `record.ast` and `record.source` so the module graph must parse from `compiled.source`.
    let src_a = SourceText::new_charged_arc(
      &mut heap,
      "a.js",
      r#"
        globalThis.__compiled_module_requires_ast_fallback_count =
          (globalThis.__compiled_module_requires_ast_fallback_count || 0) + 1;
        class C {
          #x = 1;
          getX() { return this.#x; }
        }
        export const v = (new C()).getX();
      "#,
    )?;
    let mut rec_a = SourceTextModuleRecord::compile_source(&mut heap, src_a)?;
    assert!(
      rec_a
        .compiled
        .as_ref()
        .is_some_and(|c| c.requires_ast_fallback),
      "expected private-name module to require AST fallback"
    );
    rec_a.ast = None;
    rec_a.source = None;
    let a = graph.add_module_with_specifier("a.js", rec_a)?;

    let b = graph.add_module_with_specifier(
      "b.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import { v } from "a.js";
          export const out = v;
          export const c = globalThis.__compiled_module_requires_ast_fallback_count;
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    // Link first so we can assert the AST is parsed/retained before evaluation.
    graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b)?;
    assert!(
      graph.module(a).ast.is_some(),
      "expected ModuleGraph::link to parse/retain an AST for a compiled module that requires AST fallback"
    );

    // Evaluate via the synchronous module evaluator API (exercises `eval_inner`).
    graph.evaluate_sync(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    )?;

    let mut scope = heap.scope();
    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "out")?,
      Value::Number(1.0)
    );
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "c")?,
      Value::Number(1.0),
      "module should not be evaluated twice (no partial HIR execution before AST fallback)"
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
