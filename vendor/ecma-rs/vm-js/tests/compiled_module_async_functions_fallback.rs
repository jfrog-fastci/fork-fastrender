use vm_js::{
  CompiledScript, Heap, HeapLimits, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey, Realm, Scope,
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

#[test]
fn compiled_module_does_not_fall_back_for_async_function_defs() -> Result<(), VmError> {
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
        export async function f() {
          return this === undefined ? 1 : -100;
        }

        export const g = async function () {
          return this === undefined ? 2 : -100;
        };

        export const h = async () => (this === undefined ? 3 : -100);

        export const obj = {
          async m() { return this === obj ? 4 : -100; }
        };

        export class C {
          async m() { return this instanceof C ? 5 : -100; }
        }
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
    // Drop the AST so module evaluation must use the compiled HIR path.
    record_a.ast = None;
    let a = graph.add_module_with_specifier("a.js", record_a)?;

    let b = graph.add_module_with_specifier(
      "b.js",
      SourceTextModuleRecord::parse(
        &mut heap,
        r#"
          import { f, g, h, obj, C } from "a.js";
          export let result = 0;
          const inst = new C();
          f().then(v => { result += v; });
          g().then(v => { result += v; });
          // Async arrow functions execute via call-time AST fallback; ensure they still use *lexical*
          // `this` rather than the call-site receiver (even when invoked via `.call`).
          h.call({}).then(v => { result += v; });
          obj.m().then(v => { result += v; });
          inst.m().then(v => { result += v; });
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
      ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "result")?,
      Value::Number(15.0)
    );
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
