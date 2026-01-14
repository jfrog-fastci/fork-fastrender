use vm_js::{
  CompiledScript, Heap, HeapLimits, MicrotaskQueue, ModuleGraph, PropertyKey, Realm, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext, VmOptions,
};

fn new_vm_heap_realm() -> Result<(Vm, Heap, Realm), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  // Keep this moderately small to catch leaks, but large enough for module parsing + compilation.
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
    !script.requires_ast_fallback && !script.contains_async_generators,
    "test module should compile without requiring full AST fallback"
  );
  let mut record = SourceTextModuleRecord::parse_source(heap, script.source.clone())?;
  record.compiled = Some(script);
  // Force `ModuleGraph` to instantiate/evaluate via the compiled module path.
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

#[test]
fn compiled_module_import_default_binding_executes() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "a.js",
    compile_module_record_without_ast(&mut heap, "a.js", "export default 41;")?,
  )?;
  let consumer = graph.add_module_with_specifier(
    "consumer.js",
    compile_module_record_without_ast(
      &mut heap,
      "consumer.js",
      r#"
        import x from "a.js";
        export const y = x + 1;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  let result = (|| -> Result<(), VmError> {
    let mut scope = heap.scope();
    graph.evaluate_sync_with_scope(
      &mut vm,
      &mut scope,
      realm.global_object(),
      realm.id(),
      consumer,
      &mut host,
      &mut hooks,
    )?;
    let ns_consumer = graph.get_module_namespace(consumer, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "y")?,
      Value::Number(42.0)
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
fn compiled_module_import_namespace_binding_executes() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "a.js",
    compile_module_record_without_ast(
      &mut heap,
      "a.js",
      r#"
        export const foo = 1;
        export const bar = 2;
      "#,
    )?,
  )?;
  let consumer = graph.add_module_with_specifier(
    "consumer.js",
    compile_module_record_without_ast(
      &mut heap,
      "consumer.js",
      r#"
        import * as ns from "a.js";
        export const sum = ns.foo + ns.bar;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  let result = (|| -> Result<(), VmError> {
    let mut scope = heap.scope();
    graph.evaluate_sync_with_scope(
      &mut vm,
      &mut scope,
      realm.global_object(),
      realm.id(),
      consumer,
      &mut host,
      &mut hooks,
    )?;
    let ns_consumer = graph.get_module_namespace(consumer, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(
        &mut vm,
        &mut host,
        &mut hooks,
        &mut scope,
        ns_consumer,
        "sum"
      )?,
      Value::Number(3.0)
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
fn compiled_module_import_named_alias_binding_executes() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "a.js",
    compile_module_record_without_ast(&mut heap, "a.js", "export const foo = 10;")?,
  )?;
  let consumer = graph.add_module_with_specifier(
    "consumer.js",
    compile_module_record_without_ast(
      &mut heap,
      "consumer.js",
      r#"
        import { foo as bar } from "a.js";
        export const val = bar;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  let result = (|| -> Result<(), VmError> {
    let mut scope = heap.scope();
    graph.evaluate_sync_with_scope(
      &mut vm,
      &mut scope,
      realm.global_object(),
      realm.id(),
      consumer,
      &mut host,
      &mut hooks,
    )?;
    let ns_consumer = graph.get_module_namespace(consumer, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(
        &mut vm,
        &mut host,
        &mut hooks,
        &mut scope,
        ns_consumer,
        "val"
      )?,
      Value::Number(10.0)
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
fn compiled_module_import_bindings_are_live() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "a.js",
    compile_module_record_without_ast(
      &mut heap,
      "a.js",
      r#"
        export let x = 1;
        export function inc() { x += 1; }
      "#,
    )?,
  )?;
  let consumer = graph.add_module_with_specifier(
    "consumer.js",
    compile_module_record_without_ast(
      &mut heap,
      "consumer.js",
      r#"
        import { x, inc } from "a.js";
        export const before = x;
        inc();
        export const after = x;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  let result = (|| -> Result<(), VmError> {
    let mut scope = heap.scope();
    graph.evaluate_sync_with_scope(
      &mut vm,
      &mut scope,
      realm.global_object(),
      realm.id(),
      consumer,
      &mut host,
      &mut hooks,
    )?;
    let ns_consumer = graph.get_module_namespace(consumer, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(
        &mut vm,
        &mut host,
        &mut hooks,
        &mut scope,
        ns_consumer,
        "before"
      )?,
      Value::Number(1.0)
    );
    assert_eq!(
      ns_get(
        &mut vm,
        &mut host,
        &mut hooks,
        &mut scope,
        ns_consumer,
        "after"
      )?,
      Value::Number(2.0)
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
fn compiled_module_import_binding_tdz_in_cycles() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let mut graph = ModuleGraph::new();
  let a = graph.add_module_with_specifier(
    "a.js",
    compile_module_record_without_ast(
      &mut heap,
      "a.js",
      r#"
        import { x } from "b.js";
        export function touch() { return x; }
      "#,
    )?,
  )?;
  let b = graph.add_module_with_specifier(
    "b.js",
    compile_module_record_without_ast(
      &mut heap,
      "b.js",
      r#"
        import { touch } from "a.js";
        export const before = (() => {
          try { return touch(); } catch (e) { return e.message; }
        })();
        export let x = 1;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  let result = (|| -> Result<(), VmError> {
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
    let Value::String(before_s) =
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "before")?
    else {
      return Err(VmError::InvariantViolation(
        "expected b.before to be a string",
      ));
    };
    let msg = scope.heap().get_string(before_s)?.to_utf8_lossy();
    assert!(
      msg.contains("before initialization"),
      "unexpected TDZ error message: {msg:?}"
    );

    // After evaluation, the imported binding should no longer be in the TDZ.
    let ns_a = graph.get_module_namespace(a, &mut vm, &mut scope)?;
    let touch = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_a, "touch")?;
    let touch_res = vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      touch,
      Value::Undefined,
      &[],
    )?;
    assert_eq!(touch_res, Value::Number(1.0));
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
