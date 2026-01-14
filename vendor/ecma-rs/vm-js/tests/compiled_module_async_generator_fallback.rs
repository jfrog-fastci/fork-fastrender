use vm_js::{
  CompiledScript, Heap, HeapLimits, JsRuntime, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey, Realm,
  RootId, Scope, SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext, VmOptions,
};

mod _async_generator_support;

fn new_vm_heap_realm() -> Result<(Vm, Heap, Realm), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  // Module evaluation/linking tests exercise fairly allocation-heavy paths (parsing, instantiation,
  // namespace creation). Keep the heap small to catch leaks, but not so small that minor engine
  // changes trip spurious OOM failures.
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let realm = Realm::new(&mut vm, &mut heap)?;
  Ok((vm, heap, realm))
}

fn obj_get(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  obj: vm_js::GcObject,
  name: &str,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))
}

struct JobCtx<'a> {
  vm: &'a mut Vm,
  host: &'a mut dyn VmHost,
  heap: &'a mut Heap,
}

impl VmJobContext for JobCtx<'_> {
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

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id)
  }
}

#[test]
fn compiled_modules_fall_back_to_ast_for_async_generators() -> Result<(), VmError> {
  // Skip cleanly until async generator execution is supported by the interpreter.
  // (Mirrors other async-generator tests.)
  {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;
    if !_async_generator_support::supports_async_generators(&mut rt)? {
      return Ok(());
    }
  }

  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();

  // Module `a` contains an async generator. The compiled (HIR) executor does not support executing
  // async generator bodies, so compiled-module evaluation must fall back to the AST interpreter
  // path.
  let compiled_a = CompiledScript::compile_module(
    &mut heap,
    "a.js",
    r#"
      export async function* gen() { yield 1; }
    "#,
  )?;
  let mut record_a = SourceTextModuleRecord::parse_source(compiled_a.source.clone())?;
  record_a.compiled = Some(compiled_a);
  // Drop the AST + source so the fallback path must parse on demand from the stored compiled
  // `SourceText` (`record.compiled.source`).
  record_a.ast = None;
  record_a.source = None;
  let a = graph.add_module_with_specifier("a.js", record_a)?;

  // Module `b` is compiled and imports `a.gen`, then calls `.next()` to produce a Promise.
  let compiled_b = CompiledScript::compile_module(
    &mut heap,
    "b.js",
    r#"
      import { gen } from "a.js";
      export const p = gen().next();
    "#,
  )?;
  let mut record_b = SourceTextModuleRecord::parse_source(compiled_b.source.clone())?;
  record_b.compiled = Some(compiled_b);
  // Drop the AST to ensure module evaluation actually runs through the compiled-module path.
  record_b.ast = None;
  record_b.source = None;
  let b = graph.add_module_with_specifier("b.js", record_b)?;

  graph.link_all_by_specifier();

  // Link (instantiate) before evaluating so we can assert which instantiation path each module took.
  graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b)?;

  // Ensure we took the intended code paths during instantiation:
  // - `a` must parse an AST (async-generator fallback),
  // - `b` should not require an AST (compiled path).
  assert!(
    graph.module(a).ast.is_some(),
    "expected async-generator module to fall back to AST and parse on demand"
  );
  assert!(
    graph.module(b).ast.is_none(),
    "expected compiled module without async generators to avoid AST parsing"
  );

  let eval_promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    b,
    &mut host,
    &mut hooks,
  )?;

  // Module evaluation itself should complete synchronously (it only creates a Promise via `.next()`).
  let promise_obj = {
    let mut scope = heap.scope();
    scope.push_root(eval_promise)?;
    let Value::Object(eval_promise_obj) = eval_promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    assert_eq!(
      scope.heap().promise_state(eval_promise_obj)?,
      PromiseState::Fulfilled,
      "module evaluation promise should fulfill"
    );

    // Extract the exported Promise (`b.p`).
    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    let p = obj_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "p")?;
    let Value::Object(promise_obj) = p else {
      panic!("expected b.p to be a Promise object");
    };
    scope.push_root(Value::Object(promise_obj))?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Pending);
    promise_obj
  };

  // Module `b` should still not have required parsing/retaining an AST during evaluation.
  assert!(graph.module(b).ast.is_none());

  // Drive a microtask checkpoint to settle the `.next()` Promise.
  let errors = hooks.perform_microtask_checkpoint(&mut JobCtx {
    vm: &mut vm,
    host: &mut host,
    heap: &mut heap,
  });
  if !errors.is_empty() {
    panic!("microtask checkpoint errors: {errors:?}");
  }

  let mut scope = heap.scope();
  scope.push_root(Value::Object(promise_obj))?;
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let Some(result) = scope.heap().promise_result(promise_obj)? else {
    panic!("expected fulfilled promise to have a result");
  };
  let Value::Object(result_obj) = result else {
    panic!("expected async generator .next() to fulfill with an object, got {result:?}");
  };

  assert_eq!(
    obj_get(&mut vm, &mut host, &mut hooks, &mut scope, result_obj, "value")?,
    Value::Number(1.0)
  );
  assert_eq!(
    obj_get(&mut vm, &mut host, &mut hooks, &mut scope, result_obj, "done")?,
    Value::Bool(false)
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_modules_fall_back_to_ast_for_async_generator_methods() -> Result<(), VmError> {
  // Skip cleanly until async generator execution is supported by the interpreter.
  {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;
    if !_async_generator_support::supports_async_generators(&mut rt)? {
      return Ok(());
    }
  }

  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();

  // Module `a` contains an async generator *method* (object literal). This must also force the
  // compiled-module fallback-to-AST path.
  let compiled_a = CompiledScript::compile_module(
    &mut heap,
    "a.js",
    r#"
      export const obj = {
        async *m() { yield 1; }
      };
    "#,
  )?;
  assert!(
    compiled_a.contains_async_generators,
    "expected compiled module to flag async generator methods"
  );
  let mut record_a = SourceTextModuleRecord::parse_source(compiled_a.source.clone())?;
  record_a.compiled = Some(compiled_a);
  record_a.ast = None;
  record_a.source = None;
  let a = graph.add_module_with_specifier("a.js", record_a)?;

  // Module `b` is compiled and calls the method, producing a Promise.
  let compiled_b = CompiledScript::compile_module(
    &mut heap,
    "b.js",
    r#"
      import { obj } from "a.js";
      export const p = obj.m().next();
    "#,
  )?;
  let mut record_b = SourceTextModuleRecord::parse_source(compiled_b.source.clone())?;
  record_b.compiled = Some(compiled_b);
  record_b.ast = None;
  record_b.source = None;
  let b = graph.add_module_with_specifier("b.js", record_b)?;

  graph.link_all_by_specifier();
  graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b)?;

  assert!(
    graph.module(a).ast.is_some(),
    "expected async-generator module to fall back to AST and parse on demand"
  );
  assert!(
    graph.module(b).ast.is_none(),
    "expected compiled module without async generators to avoid AST parsing"
  );

  let eval_promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    b,
    &mut host,
    &mut hooks,
  )?;

  let promise_obj = {
    let mut scope = heap.scope();
    scope.push_root(eval_promise)?;
    let Value::Object(eval_promise_obj) = eval_promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    assert_eq!(
      scope.heap().promise_state(eval_promise_obj)?,
      PromiseState::Fulfilled,
      "module evaluation promise should fulfill"
    );

    // Extract the exported Promise (`b.p`).
    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    let p = obj_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "p")?;
    let Value::Object(promise_obj) = p else {
      panic!("expected b.p to be a Promise object");
    };
    scope.push_root(Value::Object(promise_obj))?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Pending);
    promise_obj
  };

  // Drive a microtask checkpoint to settle the `.next()` Promise.
  let errors = hooks.perform_microtask_checkpoint(&mut JobCtx {
    vm: &mut vm,
    host: &mut host,
    heap: &mut heap,
  });
  if !errors.is_empty() {
    panic!("microtask checkpoint errors: {errors:?}");
  }

  let mut scope = heap.scope();
  scope.push_root(Value::Object(promise_obj))?;
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let Some(result) = scope.heap().promise_result(promise_obj)? else {
    panic!("expected fulfilled promise to have a result");
  };
  let Value::Object(result_obj) = result else {
    panic!("expected async generator .next() to fulfill with an object, got {result:?}");
  };

  assert_eq!(
    obj_get(&mut vm, &mut host, &mut hooks, &mut scope, result_obj, "value")?,
    Value::Number(1.0)
  );
  assert_eq!(
    obj_get(&mut vm, &mut host, &mut hooks, &mut scope, result_obj, "done")?,
    Value::Bool(false)
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_modules_fall_back_to_ast_for_async_generators_eval_sync() -> Result<(), VmError> {
  // Skip cleanly until async generator execution is supported by the interpreter.
  {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;
    if !_async_generator_support::supports_async_generators(&mut rt)? {
      return Ok(());
    }
  }

  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();

  let compiled_a = CompiledScript::compile_module(
    &mut heap,
    "a.js",
    r#"
      export async function* gen() { yield 1; }
    "#,
  )?;
  let mut record_a = SourceTextModuleRecord::parse_source(compiled_a.source.clone())?;
  record_a.compiled = Some(compiled_a);
  record_a.ast = None;
  record_a.source = None;
  let a = graph.add_module_with_specifier("a.js", record_a)?;

  let compiled_b = CompiledScript::compile_module(
    &mut heap,
    "b.js",
    r#"
      import { gen } from "a.js";
      export const p = gen().next();
    "#,
  )?;
  let mut record_b = SourceTextModuleRecord::parse_source(compiled_b.source.clone())?;
  record_b.compiled = Some(compiled_b);
  record_b.ast = None;
  record_b.source = None;
  let b = graph.add_module_with_specifier("b.js", record_b)?;

  graph.link_all_by_specifier();
  graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b)?;

  assert!(
    graph.module(a).ast.is_some(),
    "expected async-generator module to fall back to AST and parse on demand"
  );
  assert!(
    graph.module(b).ast.is_none(),
    "expected compiled module without async generators to avoid AST parsing"
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

  // Extract the exported Promise (`b.p`) and ensure it settles after a microtask checkpoint.
  let promise_obj = {
    let mut scope = heap.scope();
    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    let p = obj_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "p")?;
    let Value::Object(promise_obj) = p else {
      panic!("expected b.p to be a Promise object");
    };
    scope.push_root(Value::Object(promise_obj))?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Pending);
    promise_obj
  };

  let errors = hooks.perform_microtask_checkpoint(&mut JobCtx {
    vm: &mut vm,
    host: &mut host,
    heap: &mut heap,
  });
  if !errors.is_empty() {
    panic!("microtask checkpoint errors: {errors:?}");
  }

  let mut scope = heap.scope();
  scope.push_root(Value::Object(promise_obj))?;
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let Some(result) = scope.heap().promise_result(promise_obj)? else {
    panic!("expected fulfilled promise to have a result");
  };
  let Value::Object(result_obj) = result else {
    panic!("expected async generator .next() to fulfill with an object, got {result:?}");
  };
  assert_eq!(
    obj_get(&mut vm, &mut host, &mut hooks, &mut scope, result_obj, "value")?,
    Value::Number(1.0)
  );
  assert_eq!(
    obj_get(&mut vm, &mut host, &mut hooks, &mut scope, result_obj, "done")?,
    Value::Bool(false)
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}
