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
  scope.ordinary_get_with_host_and_hooks(vm, host, hooks, ns, key, Value::Object(ns))
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

  let result = (|| -> Result<(), VmError> {
    let mut graph = ModuleGraph::new();
    let m = graph.add_module_with_specifier(
      "m.js",
      SourceTextModuleRecord::parse("export const value = await Promise.resolve(42);")?,
    );
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

    Ok(())
  })();

  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  realm.teardown(&mut heap);
  result
}

#[test]
fn tla_in_dependency_makes_importer_evaluation_async() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;

  let result = (|| -> Result<(), VmError> {
    let mut graph = ModuleGraph::new();
    graph.add_module_with_specifier(
      "dep.js",
      SourceTextModuleRecord::parse("export const v = await Promise.resolve(1);")?,
    );
    let main = graph.add_module_with_specifier(
      "main.js",
      SourceTextModuleRecord::parse("import { v } from 'dep.js'; export const out = v + 1;")?,
    );
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

    Ok(())
  })();

  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  realm.teardown(&mut heap);
  result
}

#[test]
fn tla_async_cycle_evaluates_without_deadlock() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;

  let result = (|| -> Result<(), VmError> {
    let mut graph = ModuleGraph::new();
    let a = graph.add_module_with_specifier(
      "a.js",
      SourceTextModuleRecord::parse(
        r#"
          import { base } from "b.js";
          export const a = (await Promise.resolve(1)) + base();
        "#,
      )?,
    );
    let b = graph.add_module_with_specifier(
      "b.js",
      SourceTextModuleRecord::parse(
        r#"
          import { a } from "a.js";
          export function base() { return 41; }
          export const sum = a + base();
        "#,
      )?,
    );
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
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "sum")?,
      Value::Number(83.0)
    );

    Ok(())
  })();

  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  realm.teardown(&mut heap);
  result
}

#[test]
fn tla_evaluation_promise_is_cached_for_single_module() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;

  let result = (|| -> Result<(), VmError> {
    let mut graph = ModuleGraph::new();
    let m = graph.add_module_with_specifier(
      "m.js",
      SourceTextModuleRecord::parse("export const value = await Promise.resolve(42);")?,
    );
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

    Ok(())
  })();

  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  realm.teardown(&mut heap);
  result
}

#[test]
fn tla_evaluation_promise_is_cached_per_scc() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;

  let result = (|| -> Result<(), VmError> {
    let mut graph = ModuleGraph::new();
    let a = graph.add_module_with_specifier(
      "a.js",
      SourceTextModuleRecord::parse(
        r#"
          import { base } from "b.js";
          export const a = (await Promise.resolve(1)) + base();
        "#,
      )?,
    );
    let b = graph.add_module_with_specifier(
      "b.js",
      SourceTextModuleRecord::parse(
        r#"
          import { a } from "a.js";
          export function base() { return 41; }
          export const sum = a + base();
        "#,
      )?,
    );
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

    Ok(())
  })();

  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  realm.teardown(&mut heap);
  result
}

#[test]
fn tla_error_propagates_through_async_parents() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let mut promise_root: Option<RootId> = None;

  let result = (|| -> Result<(), VmError> {
    let mut graph = ModuleGraph::new();
    graph.add_module_with_specifier(
      "bad.js",
      SourceTextModuleRecord::parse("await Promise.resolve(); throw 'boom'; export const x = 1;")?,
    );
    let main = graph.add_module_with_specifier(
      "main.js",
      SourceTextModuleRecord::parse("import { x } from 'bad.js'; export const ok = 1;")?,
    );
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

    Ok(())
  })();

  if let Some(root) = promise_root {
    heap.remove_root(root);
  }
  realm.teardown(&mut heap);
  result
}

