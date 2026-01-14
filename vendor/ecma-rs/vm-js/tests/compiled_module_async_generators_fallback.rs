use vm_js::{
  CompiledScript, Heap, HeapLimits, JsRuntime, MicrotaskQueue, PromiseState, PropertyKey,
  SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext, VmOptions,
};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Compiled module evaluation allocates module graph state + Promise machinery; keep the heap
  // moderately sized so async-generator tests don't spuriously OOM as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

struct JobCtx<'a> {
  vm: &'a mut Vm,
  heap: &'a mut Heap,
  host: &'a mut dyn VmHost,
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

  fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: vm_js::RootId) {
    self.heap.remove_root(id);
  }
}

fn first_error(errors: Vec<VmError>) -> Result<(), VmError> {
  match errors.into_iter().next() {
    Some(err) => Err(err),
    None => Ok(()),
  }
}

#[test]
fn compiled_module_graph_does_not_require_full_ast_fallback_for_async_generators() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  // Module A exports an async generator; module B imports it and uses `.next()` from inside an
  // async function. Generator bodies are executed via per-function AST evaluation, so compiled
  // module execution should *not* require a full-module AST fallback just because async generator
  // syntax appears in the module.
  let compiled_a = CompiledScript::compile_module(
    rt.heap_mut(),
    "a",
    r#"
      export async function* gen() { yield 1; }
    "#,
  )?;
  let compiled_b = CompiledScript::compile_module(
    rt.heap_mut(),
    "b",
    r#"
      import { gen } from 'a';
      export async function f() {
        const it = gen();
        const r = await it.next();
        return r.value;
      }
    "#,
  )?;

  // Parse module record metadata (imports/exports) but drop the AST so the compiled-module path is
  // exercised.
  let (record_a, record_b) = {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut record_a =
      SourceTextModuleRecord::parse_source_with_vm(vm, heap, compiled_a.source.clone())?;
    record_a.compiled = Some(compiled_a);
    record_a.clear_ast();
    let mut record_b =
      SourceTextModuleRecord::parse_source_with_vm(vm, heap, compiled_b.source.clone())?;
    record_b.compiled = Some(compiled_b);
    record_b.clear_ast();
    (record_a, record_b)
  };

  let a = rt.modules_mut().add_module_with_specifier("a", record_a)?;
  let b = rt.modules_mut().add_module_with_specifier("b", record_b)?;
  rt.modules_mut().link_all_by_specifier();

  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let global_object = rt.realm().global_object();
  let realm_id = rt.realm().id();

  // Link first so we can assert no full-module AST fallback was required.
  {
    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let mut scope = heap.scope();
    modules.link_with_scope(vm, &mut scope, global_object, realm_id, b)?;
    assert!(
      modules.module(a).ast.is_none(),
      "expected async-generator module to avoid full-module AST parsing during linking"
    );
    assert!(
      modules.module(b).ast.is_none(),
      "expected compiled module without async generators to avoid AST parsing during linking"
    );
  }

  // Evaluate module B via the module graph.
  let promise_root = {
    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let mut scope = heap.scope();
    let _eval_promise = modules.evaluate_with_scope(
      vm,
      &mut scope,
      global_object,
      realm_id,
      b,
      &mut host,
      &mut hooks,
    )?;

    // Extract `f` from the module namespace and call it.
    let ns = modules.get_module_namespace(b, vm, &mut scope)?;
    scope.push_root(Value::Object(ns))?;
    let f_key = PropertyKey::from_string(scope.alloc_string("f")?);
    let f_value = scope.get_with_host_and_hooks(vm, &mut host, &mut hooks, ns, f_key, Value::Object(ns))?;
    scope.push_root(f_value)?;
    let promise = vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, f_value, Value::Undefined, &[])?;
    scope.push_root(promise)?;
    scope.heap_mut().add_root(promise)?
  };

  // Drive microtasks until `f()` settles.
  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let Value::Object(promise_obj) = heap
    .get_root(promise_root)
    .ok_or_else(|| VmError::InvariantViolation("missing rooted promise"))?
  else {
    return Err(VmError::InvariantViolation("rooted promise is not an object"));
  };

  let mut ctx = JobCtx { vm, heap, host: &mut host };

  for _ in 0..64 {
    match ctx.heap.promise_state(promise_obj)? {
      PromiseState::Fulfilled | PromiseState::Rejected => break,
      PromiseState::Pending => {}
    }

    first_error(hooks.perform_microtask_checkpoint(&mut ctx))?;
    if hooks.is_empty() {
      break;
    }
  }

  assert_eq!(ctx.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
  let value = ctx
    .heap
    .promise_result(promise_obj)?
    .ok_or_else(|| VmError::InvariantViolation("fulfilled promise missing result"))?;
  assert_eq!(value, Value::Number(1.0));

  ctx.heap.remove_root(promise_root);
  Ok(())
}
