//! Conformance tests for ECMA-262 async module evaluation across a module graph.
//!
//! These tests lock in spec-correct behavior for top-level await (TLA) when modules have
//! dependencies, cycles (SCCs), caching of evaluation promises, and error propagation.
//!
//! Run these tests explicitly with:
//!
//! ```bash
//! timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --test module_top_level_await_graph
//! ```
//!
//! Spec algorithms/sections under test (non-exhaustive):
//! - `Evaluate`
//! - `InnerModuleEvaluation`
//! - `ExecuteAsyncModule`
//! - `GatherAvailableAncestors`
//! - `AsyncModuleExecutionFulfilled` / `AsyncModuleExecutionRejected`
use vm_js::{
  GcObject, Heap, HeapLimits, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey, Realm, RootId,
  Scope, SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext, VmOptions,
};

fn new_vm_heap_realm() -> Result<(Vm, Heap, Realm), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let realm = Realm::new(&mut vm, &mut heap)?;
  Ok((vm, heap, realm))
}

struct JobCtx<'a> {
  vm: &'a mut Vm,
  heap: &'a mut Heap,
}

impl VmJobContext for JobCtx<'_> {
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

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id);
  }
}

fn drain_microtasks(vm: &mut Vm, heap: &mut Heap, hooks: &mut MicrotaskQueue) -> Result<(), VmError> {
  let errors = {
    let mut ctx = JobCtx { vm, heap };
    hooks.perform_microtask_checkpoint(&mut ctx)
  };
  if let Some(err) = errors.into_iter().next() {
    return Err(err);
  }
  Ok(())
}

fn teardown_jobs(vm: &mut Vm, heap: &mut Heap, hooks: &mut MicrotaskQueue) {
  let mut ctx = JobCtx { vm, heap };
  hooks.teardown(&mut ctx);
}

fn ns_get(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  ns: GcObject,
  name: &str,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(ns))?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.get_with_host_and_hooks(vm, host, hooks, ns, key, Value::Object(ns))
}

fn call0(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  func: Value,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(func)?;
  vm.call_with_host_and_hooks(host, &mut scope, hooks, func, Value::Undefined, &[])
}

fn root_value(heap: &mut Heap, value: Value) -> Result<RootId, VmError> {
  let mut scope = heap.scope();
  scope.push_root(value)?;
  scope.heap_mut().add_root(value)
}

fn expect_promise_object(value: Value) -> GcObject {
  match value {
    Value::Object(obj) => obj,
    _ => panic!("expected Promise object, got {value:?}"),
  }
}

#[test]
fn tla_basic_module_evaluation_promise_is_pending_until_microtasks_run() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let m = graph.add_module_with_specifier(
      "m.js",
      SourceTextModuleRecord::parse(&mut heap, "export const value = await Promise.resolve(42);")?,
    )?;
    graph.link_all_by_specifier();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "value")?,
      Value::Number(42.0)
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  // Ensure we don't leak persistent roots from queued jobs if a test exits early or if the VM
  // enqueues internal Promise jobs onto either the host-owned microtask queue or the VM-owned one.
  {
    let mut ctx = JobCtx { vm: &mut vm, heap: &mut heap };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_compiled_module_executes_via_hir_without_ast_fallback() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let source = vm_js::SourceText::new_charged_arc(
      &mut heap,
      "m.js",
      "export const value = await Promise.resolve(42);",
    )?;
    let mut record = SourceTextModuleRecord::compile_source(&mut heap, source)?;
    // Simulate an embedding that discards the `parse-js` AST after compilation.
    record.clear_ast();
    record.source = None;

    let m = graph.add_module_with_specifier("m.js", record)?;
    graph.link_all_by_specifier();

    let baseline_external = heap.vm_external_bytes();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    // If the module graph fell back to the async AST evaluator, it would parse a module AST on
    // demand and charge it against `Heap::vm_external_bytes()`.
    assert_eq!(
      heap.vm_external_bytes(),
      baseline_external,
      "expected compiled TLA evaluation to not parse/retain an AST"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "value")?,
      Value::Number(42.0)
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  {
    let mut ctx = JobCtx { vm: &mut vm, heap: &mut heap };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_compiled_module_throw_await_executes_via_hir_without_ast_fallback() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let source = vm_js::SourceText::new_charged_arc(
      &mut heap,
      "m.js",
      "throw await Promise.resolve('boom');",
    )?;
    let mut record = SourceTextModuleRecord::compile_source(&mut heap, source)?;
    // Simulate an embedding that discards the `parse-js` AST after compilation.
    record.clear_ast();
    record.source = None;

    let m = graph.add_module_with_specifier("m.js", record)?;
    graph.link_all_by_specifier();

    let baseline_external = heap.vm_external_bytes();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    // If the module graph fell back to the async AST evaluator, it would parse a module AST on
    // demand and charge it against `Heap::vm_external_bytes()`.
    assert_eq!(
      heap.vm_external_bytes(),
      baseline_external,
      "expected compiled TLA evaluation to not parse/retain an AST"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Rejected);

    let reason = heap
      .promise_result(promise_obj)?
      .expect("rejected evaluation promise should have a reason");
    let Value::String(reason_s) = reason else {
      return Err(VmError::InvariantViolation(
        "expected rejection reason to be a string",
      ));
    };
    assert_eq!(heap.get_string(reason_s)?.to_utf8_lossy(), "boom");

    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  {
    let mut ctx = JobCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_compiled_module_for_triple_head_await_executes_via_hir_without_ast_fallback() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let source = vm_js::SourceText::new_charged_arc(
      &mut heap,
      "m.js",
      r#"
        globalThis.counter = 0;
        for (await Promise.resolve(); globalThis.counter < 3; globalThis.counter++) {}
        export const out = globalThis.counter;
      "#,
    )?;
    let mut record = SourceTextModuleRecord::compile_source(&mut heap, source)?;
    // Simulate an embedding that discards the `parse-js` AST after compilation.
    record.clear_ast();
    record.source = None;

    let m = graph.add_module_with_specifier("m.js", record)?;
    graph.link_all_by_specifier();

    let baseline_external = heap.vm_external_bytes();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    // If the module graph fell back to the async AST evaluator, it would parse a module AST on
    // demand and charge it against `Heap::vm_external_bytes()`.
    assert_eq!(
      heap.vm_external_bytes(),
      baseline_external,
      "expected compiled TLA evaluation to not parse/retain an AST"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "out")?,
      Value::Number(3.0)
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  {
    let mut ctx = JobCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_compiled_module_for_triple_head_compound_assignment_await_survives_gc_without_ast_fallback(
) -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let source = vm_js::SourceText::new_charged_arc(
      &mut heap,
      "m.js",
      r#"
        export let getter_calls = 0;
        export let out = 0;
        const holder = {
          get x() {
            getter_calls++;
            return { valueOf() { return out } };
          },
          set x(v) { out = v },
        };
        for (holder.x += await Promise.resolve(1); out < 3; holder.x += await Promise.resolve(1)) {}
      "#,
    )?;
    let mut record = SourceTextModuleRecord::compile_source(&mut heap, source)?;
    // Simulate an embedding that discards the `parse-js` AST after compilation.
    record.clear_ast();
    record.source = None;

    let m = graph.add_module_with_specifier("m.js", record)?;
    graph.link_all_by_specifier();

    let baseline_external = heap.vm_external_bytes();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    // If the module graph fell back to the async AST evaluator, it would parse a module AST on
    // demand and charge it against `Heap::vm_external_bytes()`.
    assert_eq!(
      heap.vm_external_bytes(),
      baseline_external,
      "expected compiled TLA evaluation to not parse/retain an AST"
    );

    // Force a GC cycle while the module is suspended on the first `await` (in the loop init).
    // The compound assignment LHS is an ephemeral object returned from a getter, so the compiled
    // executor must keep it alive across the async boundary.
    let gc_runs_before = heap.gc_runs();
    heap.collect_garbage();
    assert!(
      heap.gc_runs() > gc_runs_before,
      "expected explicit heap GC to increment gc_runs"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "out")?,
      Value::Number(3.0)
    );
    // Init + 2 updates.
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "getter_calls")?,
      Value::Number(3.0)
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  {
    let mut ctx = JobCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_compiled_module_for_triple_head_logical_assignment_await_survives_gc_without_ast_fallback(
) -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let source = vm_js::SourceText::new_charged_arc(
      &mut heap,
      "m.js",
      r#"
        export let getter_calls = 0;
        export let out = 0;
        for (({ get x() { getter_calls++; return 0; }, set x(v) { out = v; } }).x ||= await Promise.resolve(42); out < 1; ) {}
      "#,
    )?;
    let mut record = SourceTextModuleRecord::compile_source(&mut heap, source)?;
    // Simulate an embedding that discards the `parse-js` AST after compilation.
    record.clear_ast();
    record.source = None;

    let m = graph.add_module_with_specifier("m.js", record)?;
    graph.link_all_by_specifier();

    let baseline_external = heap.vm_external_bytes();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    // If the module graph fell back to the async AST evaluator, it would parse a module AST on
    // demand and charge it against `Heap::vm_external_bytes()`.
    assert_eq!(
      heap.vm_external_bytes(),
      baseline_external,
      "expected compiled TLA evaluation to not parse/retain an AST"
    );

    // Force a GC cycle while the module is suspended on the `await` (in the loop init). The
    // assignment target base is an ephemeral object literal, so the compiled executor must keep it
    // alive across the async boundary.
    let gc_runs_before = heap.gc_runs();
    heap.collect_garbage();
    assert!(
      heap.gc_runs() > gc_runs_before,
      "expected explicit heap GC to increment gc_runs"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "out")?,
      Value::Number(42.0)
    );
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "getter_calls")?,
      Value::Number(1.0)
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  {
    let mut ctx = JobCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_compiled_module_for_triple_head_logical_assignment_short_circuits_await_rhs_without_ast_fallback(
) -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let source = vm_js::SourceText::new_charged_arc(
      &mut heap,
      "m.js",
      r#"
        export let log = "";
        export let x = 1;
        for (x ||= await (log += "R", Promise.resolve(2)); false; ) {}
        await Promise.resolve();
        log += "Z";
      "#,
    )?;
    let mut record = SourceTextModuleRecord::compile_source(&mut heap, source)?;
    // Simulate an embedding that discards the `parse-js` AST after compilation.
    record.clear_ast();
    record.source = None;

    let m = graph.add_module_with_specifier("m.js", record)?;
    graph.link_all_by_specifier();

    let baseline_external = heap.vm_external_bytes();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    // If the module graph fell back to the async AST evaluator, it would parse a module AST on
    // demand and charge it against `Heap::vm_external_bytes()`.
    assert_eq!(
      heap.vm_external_bytes(),
      baseline_external,
      "expected compiled TLA evaluation to not parse/retain an AST"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "x")?,
      Value::Number(1.0)
    );
    let Value::String(log_s) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "log")? else {
      return Err(VmError::InvariantViolation(
        "expected `log` export to be a string",
      ));
    };
    assert_eq!(scope.heap().get_string(log_s)?.to_utf8_lossy(), "Z");

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  {
    let mut ctx = JobCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_compiled_module_assignment_await_executes_via_hir_without_ast_fallback() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let source = std::sync::Arc::new(vm_js::SourceText::new_charged(
      &mut heap,
      "m.js",
      r#"
        export let x = 0;
        x = await Promise.resolve(42);
      "#,
    )?);
    let mut record = SourceTextModuleRecord::compile_source(&mut heap, source)?;
    // Simulate an embedding that discards the `parse-js` AST after compilation.
    record.clear_ast();
    record.source = None;

    let m = graph.add_module_with_specifier("m.js", record)?;
    graph.link_all_by_specifier();

    let baseline_external = heap.vm_external_bytes();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    // If the module graph fell back to the async AST evaluator, it would parse a module AST on
    // demand and charge it against `Heap::vm_external_bytes()`.
    assert_eq!(
      heap.vm_external_bytes(),
      baseline_external,
      "expected compiled TLA evaluation to not parse/retain an AST"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "x")?,
      Value::Number(42.0)
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  {
    let mut ctx = JobCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_compiled_module_destructuring_assignment_await_executes_via_hir_without_ast_fallback() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let source = vm_js::SourceText::new_charged_arc(
      &mut heap,
      "m.js",
      r#"
        export let a = 0;
        export let b = 0;
        ({ x: a } = await Promise.resolve({ x: 1 }));
        [b] = await Promise.resolve([2]);
      "#,
    )?;
    let mut record = SourceTextModuleRecord::compile_source(&mut heap, source)?;
    // Simulate an embedding that discards the `parse-js` AST after compilation.
    record.clear_ast();
    record.source = None;

    let m = graph.add_module_with_specifier("m.js", record)?;
    graph.link_all_by_specifier();

    let baseline_external = heap.vm_external_bytes();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    // If the module graph fell back to the async AST evaluator, it would parse a module AST on
    // demand and charge it against `Heap::vm_external_bytes()`.
    assert_eq!(
      heap.vm_external_bytes(),
      baseline_external,
      "expected compiled TLA evaluation to not parse/retain an AST"
    );

    // Force a GC cycle while the module is suspended on the first `await`.
    let gc_runs_before = heap.gc_runs();
    heap.collect_garbage();
    assert!(
      heap.gc_runs() > gc_runs_before,
      "expected explicit heap GC to increment gc_runs"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "a")?,
      Value::Number(1.0)
    );
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "b")?,
      Value::Number(2.0)
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  {
    let mut ctx = JobCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_compiled_module_compound_assignment_await_survives_gc_without_ast_fallback() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let source = vm_js::SourceText::new_charged_arc(
      &mut heap,
      "m.js",
      r#"
        export let getter_calls = 0;
        export let out = 0;
        const holder = {
          get x() {
            getter_calls++;
            return { valueOf() { return 40 } };
          },
          set x(v) { out = v },
        };
        holder.x += await Promise.resolve(2);
      "#,
    )?;
    let mut record = SourceTextModuleRecord::compile_source(&mut heap, source)?;
    // Simulate an embedding that discards the `parse-js` AST after compilation.
    record.clear_ast();
    record.source = None;

    let m = graph.add_module_with_specifier("m.js", record)?;
    graph.link_all_by_specifier();

    let baseline_external = heap.vm_external_bytes();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    // If the module graph fell back to the async AST evaluator, it would parse a module AST on
    // demand and charge it against `Heap::vm_external_bytes()`.
    assert_eq!(
      heap.vm_external_bytes(),
      baseline_external,
      "expected compiled TLA evaluation to not parse/retain an AST"
    );

    // Force a GC cycle while the module is suspended on the `await`. The LHS value for the
    // compound assignment is an ephemeral object returned from a getter, so the compiled executor
    // must keep it alive (and not re-read it) across the async boundary.
    let gc_runs_before = heap.gc_runs();
    heap.collect_garbage();
    assert!(
      heap.gc_runs() > gc_runs_before,
      "expected explicit heap GC to increment gc_runs"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "out")?,
      Value::Number(42.0)
    );
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "getter_calls")?,
      Value::Number(1.0)
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  {
    let mut ctx = JobCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_compiled_module_member_assignment_await_survives_gc_without_ast_fallback() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let source = std::sync::Arc::new(vm_js::SourceText::new_charged(
      &mut heap,
      "m.js",
      r#"
        export let out = 0;
        ({ set x(v) { out = v } }).x = await Promise.resolve(42);
      "#,
    )?);
    let mut record = SourceTextModuleRecord::compile_source(&mut heap, source)?;
    // Simulate an embedding that discards the `parse-js` AST after compilation.
    record.clear_ast();
    record.source = None;

    let m = graph.add_module_with_specifier("m.js", record)?;
    graph.link_all_by_specifier();

    let baseline_external = heap.vm_external_bytes();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    // If the module graph fell back to the async AST evaluator, it would parse a module AST on
    // demand and charge it against `Heap::vm_external_bytes()`.
    assert_eq!(
      heap.vm_external_bytes(),
      baseline_external,
      "expected compiled TLA evaluation to not parse/retain an AST"
    );

    // Force a GC cycle while the module is suspended on the `await`. The assignment target base is
    // an ephemeral object literal, so the compiled executor must keep it alive across the async
    // boundary.
    let gc_runs_before = heap.gc_runs();
    heap.collect_garbage();
    assert!(
      heap.gc_runs() > gc_runs_before,
      "expected explicit heap GC to increment gc_runs"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "out")?,
      Value::Number(42.0)
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  {
    let mut ctx = JobCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_compiled_module_logical_or_assignment_await_survives_gc_without_ast_fallback() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let source = vm_js::SourceText::new_charged_arc(
      &mut heap,
      "m.js",
      r#"
        export let getter_calls = 0;
        export let out = 0;
        ({ get x() { getter_calls++; return 0 }, set x(v) { out = v } }).x ||= await Promise.resolve(42);
      "#,
    )?;
    let mut record = SourceTextModuleRecord::compile_source(&mut heap, source)?;
    // Simulate an embedding that discards the `parse-js` AST after compilation.
    record.clear_ast();
    record.source = None;

    let m = graph.add_module_with_specifier("m.js", record)?;
    graph.link_all_by_specifier();

    let baseline_external = heap.vm_external_bytes();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    // If the module graph fell back to the async AST evaluator, it would parse a module AST on
    // demand and charge it against `Heap::vm_external_bytes()`.
    assert_eq!(
      heap.vm_external_bytes(),
      baseline_external,
      "expected compiled TLA evaluation to not parse/retain an AST"
    );

    // Force a GC cycle while the module is suspended on the `await`. The assignment target base is
    // an ephemeral object literal, so the compiled executor must keep it alive across the async
    // boundary.
    let gc_runs_before = heap.gc_runs();
    heap.collect_garbage();
    assert!(
      heap.gc_runs() > gc_runs_before,
      "expected explicit heap GC to increment gc_runs"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "out")?,
      Value::Number(42.0)
    );
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "getter_calls")?,
      Value::Number(1.0)
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  {
    let mut ctx = JobCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_compiled_module_logical_assignment_short_circuits_await_rhs_without_ast_fallback() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let source = vm_js::SourceText::new_charged_arc(
      &mut heap,
      "m.js",
      r#"
        export let log = "";
        export let a = 1;
        export let b = 0;
        export let c = 0;

        a ||= await (log += "A", Promise.resolve(2));
        b &&= await (log += "B", Promise.resolve(3));
        c ??= await (log += "C", Promise.resolve(4));

        await Promise.resolve();
        log += "Z";
      "#,
    )?;
    let mut record = SourceTextModuleRecord::compile_source(&mut heap, source)?;
    // Simulate an embedding that discards the `parse-js` AST after compilation.
    record.clear_ast();
    record.source = None;

    let m = graph.add_module_with_specifier("m.js", record)?;
    graph.link_all_by_specifier();

    let baseline_external = heap.vm_external_bytes();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    // If the module graph fell back to the async AST evaluator, it would parse a module AST on
    // demand and charge it against `Heap::vm_external_bytes()`.
    assert_eq!(
      heap.vm_external_bytes(),
      baseline_external,
      "expected compiled TLA evaluation to not parse/retain an AST"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(m, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "a")?,
      Value::Number(1.0)
    );
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "b")?,
      Value::Number(0.0)
    );
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "c")?,
      Value::Number(0.0)
    );

    let Value::String(log_s) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "log")? else {
      return Err(VmError::InvariantViolation(
        "expected `log` export to be a string",
      ));
    };
    assert_eq!(scope.heap().get_string(log_s)?.to_utf8_lossy(), "Z");

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  {
    let mut ctx = JobCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_in_dependency_makes_importer_evaluation_async() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    graph.add_module_with_specifier(
      "dep.js",
      SourceTextModuleRecord::parse(&mut heap, "export const v = await Promise.resolve(1);")?,
    )?;
    let main = graph.add_module_with_specifier(
      "main.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        "import { v } from 'dep.js'; export const out = v + 1;",
      )?,
    )?;
    graph.link_all_by_specifier();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      main,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(main, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "out")?,
      Value::Number(2.0)
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  result
}

#[test]
fn tla_evaluate_is_idempotent_for_importer_without_tla() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    graph.add_module_with_specifier(
      "dep.js",
      SourceTextModuleRecord::parse(&mut heap, "export const v = await Promise.resolve(1);")?,
    )?;
    let main = graph.add_module_with_specifier(
      "main.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        "import { v } from 'dep.js'; export const out = v + 1;",
      )?,
    )?;
    graph.link_all_by_specifier();

    let promise1 = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      main,
      &mut host,
      &mut hooks,
    )?;
    let promise1_obj = expect_promise_object(promise1);

    promise_root = Some(root_value(&mut heap, promise1)?);
    assert_eq!(heap.promise_state(promise1_obj)?, PromiseState::Pending);

    let promise2 = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      main,
      &mut host,
      &mut hooks,
    )?;
    let promise2_obj = expect_promise_object(promise2);

    assert_eq!(
      promise1_obj, promise2_obj,
      "evaluating the importer module twice should return the same Promise object"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(main, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "out")?,
      Value::Number(2.0)
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  result
}

#[test]
fn tla_async_parent_order_follows_import_order_not_module_id() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    // Add modules in a non-import order to ensure ModuleId assignment does not match import order.
    graph.add_module_with_specifier(
      "./dep.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          globalThis.order = [];
          await Promise.resolve();
          globalThis.order.push('dep');
          export const x = 1;
        "#,
      )?,
    )?;
    graph.add_module_with_specifier(
      "./c.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import './dep.js';
          globalThis.order.push('c');
          export {};
        "#,
      )?,
    )?;
    graph.add_module_with_specifier(
      "./b.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import './dep.js';
          globalThis.order.push('b');
          export {};
        "#,
      )?,
    )?;
    let main = graph.add_module_with_specifier(
      "./main.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import './b.js';
          import './c.js';
          export const order = globalThis.order.join(',');
        "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      main,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(main, &mut vm, &mut scope)?;
    let Value::String(order_s) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "order")?
    else {
      return Err(VmError::InvariantViolation(
        "expected `order` export to be a string",
      ));
    };
    assert_eq!(scope.heap().get_string(order_s)?.to_utf8_lossy(), "dep,b,c");

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  result
}

#[test]
fn tla_async_cycle_evaluates_without_deadlock() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let a = graph.add_module_with_specifier(
      "a.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import { base } from "b.js";
          export const a = (await Promise.resolve(1)) + base();
        "#,
      )?,
    )?;
    let b = graph.add_module_with_specifier(
      "b.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
           import { a } from "a.js";
           export function base() { return 41; }
           export function sum() { return a + base(); }
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
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(
      heap.promise_state(promise_obj)?,
      PromiseState::Fulfilled,
      "async cycle evaluation promise did not settle"
    );

    let mut scope = heap.scope();
    let ns_a = graph.get_module_namespace(a, &mut vm, &mut scope)?;
    let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_a, "a")?,
      Value::Number(42.0)
    );
    assert_eq!(
      {
        let sum = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "sum")?;
        call0(&mut vm, &mut host, &mut hooks, &mut scope, sum)?
      },
      Value::Number(83.0),
      "calling `sum()` should observe `a` after async cycle evaluation"
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  result
}

#[test]
fn tla_evaluation_promise_is_cached_for_single_module() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let m = graph.add_module_with_specifier(
      "m.js",
      SourceTextModuleRecord::parse(&mut heap, "export const value = await Promise.resolve(42);")?,
    )?;
    graph.link_all_by_specifier();

    let promise1 = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise1_obj = expect_promise_object(promise1);

    promise_root = Some(root_value(&mut heap, promise1)?);

    let promise2 = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut host,
      &mut hooks,
    )?;
    let promise2_obj = expect_promise_object(promise2);
    assert_eq!(
      promise1_obj, promise2_obj,
      "evaluating the same module twice should return the same Promise object"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  result
}

#[test]
fn tla_evaluation_promise_is_cached_per_scc() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    let a = graph.add_module_with_specifier(
      "a.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import { base } from "b.js";
          export const a = (await Promise.resolve(1)) + base();
        "#,
      )?,
    )?;
    let b = graph.add_module_with_specifier(
      "b.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
           import { a } from "a.js";
           export function base() { return 41; }
           export function sum() { return a + base(); }
         "#,
      )?,
    )?;
    graph.link_all_by_specifier();

    let promise_a = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      a,
      &mut host,
      &mut hooks,
    )?;
    let promise_a_obj = expect_promise_object(promise_a);

    promise_root = Some(root_value(&mut heap, promise_a)?);

    let promise_b = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut host,
      &mut hooks,
    )?;
    let promise_b_obj = expect_promise_object(promise_b);
    assert_eq!(
      promise_a_obj, promise_b_obj,
      "modules in the same async SCC should share the same evaluation promise"
    );

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  // The host-owned `MicrotaskQueue` contains `Job`s which may hold persistent roots. Ensure any
  // remaining jobs are discarded before dropping the queue, and also tear down the VM-owned queue.
  // This keeps debug assertions in `Job::drop` from firing even if the test doesn't fully drain
  // microtasks.
  {
    let mut ctx = JobCtx { vm: &mut vm, heap: &mut heap };
    hooks.teardown(&mut ctx);
  }
  vm.teardown_microtasks(&mut heap);
  result
}

#[test]
fn tla_error_propagates_through_async_parents() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;
  let mut graph = ModuleGraph::new();

  let result = (|| -> Result<(), VmError> {
    graph.add_module_with_specifier(
      "bad.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        "await Promise.resolve(); throw 'boom'; export const x = 1;",
      )?,
    )?;
    let main = graph.add_module_with_specifier(
      "main.js",
      SourceTextModuleRecord::parse(&mut heap, "import { x } from 'bad.js'; export const ok = 1;")?,
    )?;
    graph.link_all_by_specifier();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      main,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Rejected);

    let reason = heap
      .promise_result(promise_obj)?
      .expect("rejected evaluation promise should have a reason");
    let Value::String(reason_s) = reason else {
      panic!("expected rejection reason to be a string, got {reason:?}");
    };
    assert_eq!(heap.get_string(reason_s)?.to_utf8_lossy(), "boom");

    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  graph.teardown(&mut vm, &mut heap);
  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  graph.teardown(&mut vm, &mut heap);
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  result
}

#[test]
fn tla_reexport_from_dependency_is_awaited() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;

  let result = (|| -> Result<(), VmError> {
    let mut graph = ModuleGraph::new();
    graph.add_module_with_specifier(
      "dep.js",
      SourceTextModuleRecord::parse(&mut heap, "export const v = await Promise.resolve(7);")?,
    )?;
    let reexport = graph.add_module_with_specifier(
      "reexport.js",
      SourceTextModuleRecord::parse(&mut heap, "export { v } from 'dep.js';")?,
    )?;
    graph.link_all_by_specifier();

    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      reexport,
      &mut host,
      &mut hooks,
    )?;
    let promise_obj = expect_promise_object(promise);

    promise_root = Some(root_value(&mut heap, promise)?);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

    drain_microtasks(&mut vm, &mut heap, &mut hooks)?;

    let promise = heap
      .get_root(promise_root.ok_or_else(|| VmError::InvariantViolation("promise root missing"))?)
      .ok_or_else(VmError::invalid_handle)?;
    let promise_obj = expect_promise_object(promise);
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

    let mut scope = heap.scope();
    let ns = graph.get_module_namespace(reexport, &mut vm, &mut scope)?;
    assert_eq!(
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "v")?,
      Value::Number(7.0)
    );

    drop(scope);
    graph.teardown(&mut vm, &mut heap);
    Ok(())
  })();

  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  teardown_jobs(&mut vm, &mut heap, &mut hooks);
  realm.teardown(&mut heap);
  result
}
