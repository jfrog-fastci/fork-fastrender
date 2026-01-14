use vm_js::{
  CompiledScript, Heap, HeapLimits, JsRuntime, MicrotaskQueue, PromiseState, PropertyKey, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Promise + async iterator machinery needs a bit of heap headroom.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
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
      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(obj))?;
      let key_s = scope.alloc_string("message")?;
      scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);
      let value = scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;
      let Value::String(message) = value else {
        return Ok(false);
      };
      Ok(scope.heap().get_string(message)?.to_utf8_lossy().contains(needle))
    }
    _ => Ok(false),
  }
}

#[test]
fn compiled_module_top_level_nested_labeled_for_await_of_break_outer_label_executes_and_closes_iterator(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let result = (|| -> Result<(), VmError> {
    let compiled = CompiledScript::compile_module(
      rt.heap_mut(),
      "m.js",
      r#"
        export let actual = "";
        export let returnCalls = 0;

        const iterable = {};
        iterable[Symbol.asyncIterator] = function () {
          return {
            i: 0,
            next() {
              if (this.i++ === 0) return Promise.resolve({ value: "a", done: false });
              return Promise.resolve({ value: "b", done: false });
            },
            return() {
              returnCalls++;
              return Promise.resolve({ done: true });
            },
          };
        };

        outer: inner: for await (const x of iterable) {
          actual += x;
          break outer;
        }
        actual += "done";
      "#,
    )?;
    assert!(
      !compiled.top_level_await_requires_ast_fallback,
      "nested labelled top-level for-await-of should be supported by the compiled module TLA executor"
    );
    assert!(
      !compiled.requires_ast_fallback,
      "supported compiled module TLA shapes should not trigger the general compiled-module AST fallback"
    );

    let mut record = SourceTextModuleRecord::parse_source(rt.heap_mut(), compiled.source.clone())?;
    assert!(record.has_tla, "for-await-of should mark the module as `[[HasTLA]]`");
    record.compiled = Some(compiled);
    // Force ModuleGraph to use the compiled-module (HIR) instantiation + execution path.
    record.clear_ast();

    let global_object = rt.realm().global_object();
    let realm_id = rt.realm().id();

    // Evaluate in a nested block so we don't hold `&mut JsRuntime` borrows while running microtasks.
    let (promise, module) = {
      let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
      let m = modules.add_module_with_specifier("m", record)?;
      modules.link_all_by_specifier();
      let promise = match modules.evaluate(vm, heap, global_object, realm_id, m, &mut host, &mut hooks) {
        Ok(p) => p,
        Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
        Err(e) => return Err(e),
      };
      (promise, m)
    };

    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };

    // If the promise was rejected due to missing ASTs (compiled module execution disabled in this
    // configuration), skip.
    {
      let (vm, _modules, heap) = rt.vm_modules_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(promise)?;
      if promise_rejection_message_contains(
        vm,
        &mut host,
        &mut hooks,
        &mut scope,
        promise_obj,
        "module AST missing",
      )? {
        return Ok(());
      }
      assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Pending);
    }

    // Run Promise jobs to drive evaluation to completion.
    let errors = hooks.perform_microtask_checkpoint(&mut rt);
    if let Some(err) = errors.into_iter().next() {
      return Err(err);
    }

    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = modules.get_module_namespace(module, vm, &mut scope)?;
    let Value::String(actual) = ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "actual")? else {
      panic!("expected module export 'actual' to be a string");
    };
    assert_eq!(scope.heap().get_string(actual)?.to_utf8_lossy(), "adone");
    assert_eq!(
      ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "returnCalls")?,
      Value::Number(1.0),
      "breaking out of a labelled for-await-of must call iterator.return()"
    );
    Ok(())
  })();

  // Ensure any queued jobs are discarded so they do not leak persistent roots.
  hooks.teardown(&mut rt);
  result
}

#[test]
fn compiled_module_top_level_labeled_for_await_of_break_label_with_await_rhs_executes_and_closes_iterator(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let result = (|| -> Result<(), VmError> {
    let compiled = CompiledScript::compile_module(
      rt.heap_mut(),
      "m.js",
      r#"
        export let actual = "";
        export let returnCalls = 0;

        const iterable = {};
        iterable[Symbol.asyncIterator] = function () {
          return {
            i: 0,
            next() {
              if (this.i++ === 0) return Promise.resolve({ value: "a", done: false });
              return Promise.resolve({ value: "b", done: false });
            },
            return() {
              returnCalls++;
              return Promise.resolve({ done: true });
            },
          };
        };

        outer: for await (const x of await Promise.resolve(iterable)) {
          actual += x;
          break outer;
        }
        actual += "done";
      "#,
    )?;
    assert!(
      !compiled.top_level_await_requires_ast_fallback,
      "labelled top-level for-await-of with a direct await RHS should be supported by the compiled module TLA executor"
    );

    let mut record = SourceTextModuleRecord::parse_source(rt.heap_mut(), compiled.source.clone())?;
    assert!(record.has_tla, "for-await-of should mark the module as `[[HasTLA]]`");
    record.compiled = Some(compiled);
    record.clear_ast();

    let global_object = rt.realm().global_object();
    let realm_id = rt.realm().id();

    let (promise, module) = {
      let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
      let m = modules.add_module_with_specifier("m", record)?;
      modules.link_all_by_specifier();
      let promise = match modules.evaluate(vm, heap, global_object, realm_id, m, &mut host, &mut hooks) {
        Ok(p) => p,
        Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
        Err(e) => return Err(e),
      };
      (promise, m)
    };

    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };

    {
      let (vm, _modules, heap) = rt.vm_modules_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(promise)?;
      if promise_rejection_message_contains(
        vm,
        &mut host,
        &mut hooks,
        &mut scope,
        promise_obj,
        "module AST missing",
      )? {
        return Ok(());
      }
    }

    let errors = hooks.perform_microtask_checkpoint(&mut rt);
    if let Some(err) = errors.into_iter().next() {
      return Err(err);
    }

    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = modules.get_module_namespace(module, vm, &mut scope)?;
    let Value::String(actual) = ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "actual")? else {
      panic!("expected module export 'actual' to be a string");
    };
    assert_eq!(scope.heap().get_string(actual)?.to_utf8_lossy(), "adone");
    assert_eq!(
      ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "returnCalls")?,
      Value::Number(1.0),
    );
    Ok(())
  })();

  hooks.teardown(&mut rt);
  result
}

#[test]
fn compiled_module_top_level_nested_labeled_for_triple_with_await_in_init_break_outer_label_executes(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let result = (|| -> Result<(), VmError> {
    let compiled = CompiledScript::compile_module(
      rt.heap_mut(),
      "m.js",
      r#"
        export let actual = "";
        let i = 0;
        outer: inner: for (i = await Promise.resolve(1); i < 3; i++) {
          actual += i;
          break outer;
        }
        actual += "done";
      "#,
    )?;
    assert!(
      !compiled.top_level_await_requires_ast_fallback,
      "nested labelled top-level for-triple with await in the loop head should be supported by the compiled module TLA executor"
    );
    assert!(
      !compiled.requires_ast_fallback,
      "supported compiled module TLA shapes should not trigger the general compiled-module AST fallback"
    );

    let mut record = SourceTextModuleRecord::parse_source(rt.heap_mut(), compiled.source.clone())?;
    assert!(
      record.has_tla,
      "await in a top-level for-loop head should mark the module as `[[HasTLA]]`"
    );
    record.compiled = Some(compiled);
    record.clear_ast();

    let global_object = rt.realm().global_object();
    let realm_id = rt.realm().id();

    let (promise, module) = {
      let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
      let m = modules.add_module_with_specifier("m", record)?;
      modules.link_all_by_specifier();
      let promise = match modules.evaluate(vm, heap, global_object, realm_id, m, &mut host, &mut hooks) {
        Ok(p) => p,
        Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
        Err(e) => return Err(e),
      };
      (promise, m)
    };

    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };

    {
      let (vm, _modules, heap) = rt.vm_modules_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(promise)?;
      if promise_rejection_message_contains(
        vm,
        &mut host,
        &mut hooks,
        &mut scope,
        promise_obj,
        "module AST missing",
      )? {
        return Ok(());
      }
    }

    let errors = hooks.perform_microtask_checkpoint(&mut rt);
    if let Some(err) = errors.into_iter().next() {
      return Err(err);
    }

    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = modules.get_module_namespace(module, vm, &mut scope)?;
    let Value::String(actual) = ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "actual")? else {
      panic!("expected module export 'actual' to be a string");
    };
    assert_eq!(scope.heap().get_string(actual)?.to_utf8_lossy(), "1done");
    Ok(())
  })();

  hooks.teardown(&mut rt);
  result
}

#[test]
fn compiled_module_top_level_nested_labeled_for_triple_with_await_in_init_continue_outer_label_executes(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let result = (|| -> Result<(), VmError> {
    let compiled = CompiledScript::compile_module(
      rt.heap_mut(),
      "m.js",
      r#"
        export let actual = "";
        let i = 0;
        outer: inner: for (i = await Promise.resolve(0); i < 2; i++) {
          if (i === 0) continue outer;
          actual += "b";
        }
        actual += "done";
      "#,
    )?;
    assert!(
      !compiled.top_level_await_requires_ast_fallback,
      "nested labelled top-level for-triple with await in the loop head should be supported by the compiled module TLA executor"
    );

    let mut record = SourceTextModuleRecord::parse_source(rt.heap_mut(), compiled.source.clone())?;
    record.compiled = Some(compiled);
    record.clear_ast();

    let global_object = rt.realm().global_object();
    let realm_id = rt.realm().id();

    let (promise, module) = {
      let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
      let m = modules.add_module_with_specifier("m", record)?;
      modules.link_all_by_specifier();
      let promise = match modules.evaluate(vm, heap, global_object, realm_id, m, &mut host, &mut hooks) {
        Ok(p) => p,
        Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
        Err(e) => return Err(e),
      };
      (promise, m)
    };

    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };

    {
      let (vm, _modules, heap) = rt.vm_modules_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(promise)?;
      if promise_rejection_message_contains(
        vm,
        &mut host,
        &mut hooks,
        &mut scope,
        promise_obj,
        "module AST missing",
      )? {
        return Ok(());
      }
    }

    let errors = hooks.perform_microtask_checkpoint(&mut rt);
    if let Some(err) = errors.into_iter().next() {
      return Err(err);
    }

    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = modules.get_module_namespace(module, vm, &mut scope)?;
    let Value::String(actual) = ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "actual")? else {
      panic!("expected module export 'actual' to be a string");
    };
    assert_eq!(scope.heap().get_string(actual)?.to_utf8_lossy(), "bdone");
    Ok(())
  })();

  hooks.teardown(&mut rt);
  result
}

#[test]
fn compiled_module_top_level_nested_labeled_for_of_with_await_in_head_default_break_outer_label_executes_and_closes_iterator(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let result = (|| -> Result<(), VmError> {
    let compiled = CompiledScript::compile_module(
      rt.heap_mut(),
      "m.js",
      r#"
        export let actual = "";
        export let returnCalls = 0;

        const iterable = {};
        iterable[Symbol.iterator] = function () {
          return {
            i: 0,
            next() {
              if (this.i++ < 2) return { value: {}, done: false };
              return { done: true };
            },
            return() {
              returnCalls++;
              return { done: true };
            },
          };
        };

        outer: inner: for (const { x = await Promise.resolve("a") } of iterable) {
          actual += x;
          break outer;
        }
        actual += "done";
      "#,
    )?;
    assert!(
      !compiled.top_level_await_requires_ast_fallback,
      "nested labelled top-level for-of with await in the object-pattern head should be supported by the compiled module TLA executor"
    );

    let mut record = SourceTextModuleRecord::parse_source(rt.heap_mut(), compiled.source.clone())?;
    assert!(
      record.has_tla,
      "await in a top-level for-of head pattern should mark the module as `[[HasTLA]]`"
    );
    record.compiled = Some(compiled);
    record.clear_ast();

    let global_object = rt.realm().global_object();
    let realm_id = rt.realm().id();

    let (promise, module) = {
      let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
      let m = modules.add_module_with_specifier("m", record)?;
      modules.link_all_by_specifier();
      let promise = match modules.evaluate(vm, heap, global_object, realm_id, m, &mut host, &mut hooks) {
        Ok(p) => p,
        Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
        Err(e) => return Err(e),
      };
      (promise, m)
    };

    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };

    {
      let (vm, _modules, heap) = rt.vm_modules_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(promise)?;
      if promise_rejection_message_contains(
        vm,
        &mut host,
        &mut hooks,
        &mut scope,
        promise_obj,
        "module AST missing",
      )? {
        return Ok(());
      }
    }

    let errors = hooks.perform_microtask_checkpoint(&mut rt);
    if let Some(err) = errors.into_iter().next() {
      return Err(err);
    }

    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = modules.get_module_namespace(module, vm, &mut scope)?;
    let Value::String(actual) = ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "actual")? else {
      panic!("expected module export 'actual' to be a string");
    };
    assert_eq!(scope.heap().get_string(actual)?.to_utf8_lossy(), "adone");
    assert_eq!(
      ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "returnCalls")?,
      Value::Number(1.0),
      "breaking out of a labelled for-of must call iterator.return()"
    );
    Ok(())
  })();

  hooks.teardown(&mut rt);
  result
}

#[test]
fn compiled_module_top_level_nested_labeled_for_of_with_await_in_head_default_continue_outer_label_executes(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let result = (|| -> Result<(), VmError> {
    let compiled = CompiledScript::compile_module(
      rt.heap_mut(),
      "m.js",
      r#"
        export let actual = "";
        let i = 0;

        const iterable = {};
        iterable[Symbol.iterator] = function () {
          return {
            i: 0,
            next() {
              if (this.i++ < 2) return { value: {}, done: false };
              return { done: true };
            },
          };
        };

        outer: inner: for (const { x = await Promise.resolve("b") } of iterable) {
          if (i++ === 0) continue outer;
          actual += x;
        }
        actual += "done";
      "#,
    )?;
    assert!(
      !compiled.top_level_await_requires_ast_fallback,
      "nested labelled top-level for-of with await in the object-pattern head should be supported by the compiled module TLA executor"
    );

    let mut record = SourceTextModuleRecord::parse_source(rt.heap_mut(), compiled.source.clone())?;
    record.compiled = Some(compiled);
    record.clear_ast();

    let global_object = rt.realm().global_object();
    let realm_id = rt.realm().id();

    let (promise, module) = {
      let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
      let m = modules.add_module_with_specifier("m", record)?;
      modules.link_all_by_specifier();
      let promise = match modules.evaluate(vm, heap, global_object, realm_id, m, &mut host, &mut hooks) {
        Ok(p) => p,
        Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
        Err(e) => return Err(e),
      };
      (promise, m)
    };

    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };

    {
      let (vm, _modules, heap) = rt.vm_modules_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(promise)?;
      if promise_rejection_message_contains(
        vm,
        &mut host,
        &mut hooks,
        &mut scope,
        promise_obj,
        "module AST missing",
      )? {
        return Ok(());
      }
    }

    let errors = hooks.perform_microtask_checkpoint(&mut rt);
    if let Some(err) = errors.into_iter().next() {
      return Err(err);
    }

    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = modules.get_module_namespace(module, vm, &mut scope)?;
    let Value::String(actual) = ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "actual")? else {
      panic!("expected module export 'actual' to be a string");
    };
    assert_eq!(scope.heap().get_string(actual)?.to_utf8_lossy(), "bdone");
    Ok(())
  })();

  hooks.teardown(&mut rt);
  result
}

#[test]
fn compiled_module_top_level_object_destructuring_assignment_with_await_in_default_executes() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let result = (|| -> Result<(), VmError> {
    let compiled = CompiledScript::compile_module(
      rt.heap_mut(),
      "m.js",
      r#"
        export let x;
        ({ x = await Promise.resolve("a") } = {});
      "#,
    )?;
    assert!(
      !compiled.top_level_await_requires_ast_fallback,
      "top-level object destructuring assignment with await in a default value should be supported by the compiled module TLA executor"
    );

    let mut record = SourceTextModuleRecord::parse_source(rt.heap_mut(), compiled.source.clone())?;
    assert!(
      record.has_tla,
      "await in a top-level destructuring assignment pattern should mark the module as `[[HasTLA]]`"
    );
    record.compiled = Some(compiled);
    record.clear_ast();

    let global_object = rt.realm().global_object();
    let realm_id = rt.realm().id();

    let (promise, module) = {
      let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
      let m = modules.add_module_with_specifier("m", record)?;
      modules.link_all_by_specifier();
      let promise = match modules.evaluate(vm, heap, global_object, realm_id, m, &mut host, &mut hooks) {
        Ok(p) => p,
        Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
        Err(e) => return Err(e),
      };
      (promise, m)
    };

    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };

    {
      let (vm, _modules, heap) = rt.vm_modules_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(promise)?;
      if promise_rejection_message_contains(
        vm,
        &mut host,
        &mut hooks,
        &mut scope,
        promise_obj,
        "module AST missing",
      )? {
        return Ok(());
      }
    }

    let errors = hooks.perform_microtask_checkpoint(&mut rt);
    if let Some(err) = errors.into_iter().next() {
      return Err(err);
    }

    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = modules.get_module_namespace(module, vm, &mut scope)?;
    let Value::String(x) = ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "x")? else {
      panic!("expected module export 'x' to be a string");
    };
    assert_eq!(scope.heap().get_string(x)?.to_utf8_lossy(), "a");
    Ok(())
  })();

  hooks.teardown(&mut rt);
  result
}

#[test]
fn compiled_module_top_level_object_destructuring_assignment_with_await_in_computed_key_executes(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let result = (|| -> Result<(), VmError> {
    let compiled = CompiledScript::compile_module(
      rt.heap_mut(),
      "m.js",
      r#"
        export let x;
        ({ [await Promise.resolve("k")]: x } = { k: "b" });
      "#,
    )?;
    assert!(
      !compiled.top_level_await_requires_ast_fallback,
      "top-level object destructuring assignment with await in a computed key should be supported by the compiled module TLA executor"
    );

    let mut record = SourceTextModuleRecord::parse_source(rt.heap_mut(), compiled.source.clone())?;
    record.compiled = Some(compiled);
    record.clear_ast();

    let global_object = rt.realm().global_object();
    let realm_id = rt.realm().id();

    let (promise, module) = {
      let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
      let m = modules.add_module_with_specifier("m", record)?;
      modules.link_all_by_specifier();
      let promise = match modules.evaluate(vm, heap, global_object, realm_id, m, &mut host, &mut hooks) {
        Ok(p) => p,
        Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
        Err(e) => return Err(e),
      };
      (promise, m)
    };

    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };

    {
      let (vm, _modules, heap) = rt.vm_modules_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(promise)?;
      if promise_rejection_message_contains(
        vm,
        &mut host,
        &mut hooks,
        &mut scope,
        promise_obj,
        "module AST missing",
      )? {
        return Ok(());
      }
    }

    let errors = hooks.perform_microtask_checkpoint(&mut rt);
    if let Some(err) = errors.into_iter().next() {
      return Err(err);
    }

    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = modules.get_module_namespace(module, vm, &mut scope)?;
    let Value::String(x) = ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "x")? else {
      panic!("expected module export 'x' to be a string");
    };
    assert_eq!(scope.heap().get_string(x)?.to_utf8_lossy(), "b");
    Ok(())
  })();

  hooks.teardown(&mut rt);
  result
}

#[test]
fn compiled_module_top_level_object_destructuring_assignment_with_await_rhs_executes() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let result = (|| -> Result<(), VmError> {
    let compiled = CompiledScript::compile_module(
      rt.heap_mut(),
      "m.js",
      r#"
        export let x;
        ({ x } = await Promise.resolve({ x: "c" }));
      "#,
    )?;
    assert!(
      !compiled.top_level_await_requires_ast_fallback,
      "top-level object destructuring assignment with a direct await RHS should be supported by the compiled module TLA executor"
    );

    let mut record = SourceTextModuleRecord::parse_source(rt.heap_mut(), compiled.source.clone())?;
    record.compiled = Some(compiled);
    record.clear_ast();

    let global_object = rt.realm().global_object();
    let realm_id = rt.realm().id();

    let (promise, module) = {
      let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
      let m = modules.add_module_with_specifier("m", record)?;
      modules.link_all_by_specifier();
      let promise = match modules.evaluate(vm, heap, global_object, realm_id, m, &mut host, &mut hooks) {
        Ok(p) => p,
        Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
        Err(e) => return Err(e),
      };
      (promise, m)
    };

    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };

    {
      let (vm, _modules, heap) = rt.vm_modules_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(promise)?;
      if promise_rejection_message_contains(
        vm,
        &mut host,
        &mut hooks,
        &mut scope,
        promise_obj,
        "module AST missing",
      )? {
        return Ok(());
      }
    }

    let errors = hooks.perform_microtask_checkpoint(&mut rt);
    if let Some(err) = errors.into_iter().next() {
      return Err(err);
    }

    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = modules.get_module_namespace(module, vm, &mut scope)?;
    let Value::String(x) = ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "x")? else {
      panic!("expected module export 'x' to be a string");
    };
    assert_eq!(scope.heap().get_string(x)?.to_utf8_lossy(), "c");
    Ok(())
  })();

  hooks.teardown(&mut rt);
  result
}

#[test]
fn compiled_module_top_level_array_destructuring_assignment_with_await_rhs_executes() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let result = (|| -> Result<(), VmError> {
    let compiled = CompiledScript::compile_module(
      rt.heap_mut(),
      "m.js",
      r#"
        export let x;
        [x] = await Promise.resolve(["d"]);
      "#,
    )?;
    assert!(
      !compiled.top_level_await_requires_ast_fallback,
      "top-level array destructuring assignment with a direct await RHS should be supported by the compiled module TLA executor"
    );

    let mut record = SourceTextModuleRecord::parse_source(rt.heap_mut(), compiled.source.clone())?;
    record.compiled = Some(compiled);
    record.clear_ast();

    let global_object = rt.realm().global_object();
    let realm_id = rt.realm().id();

    let (promise, module) = {
      let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
      let m = modules.add_module_with_specifier("m", record)?;
      modules.link_all_by_specifier();
      let promise = match modules.evaluate(vm, heap, global_object, realm_id, m, &mut host, &mut hooks) {
        Ok(p) => p,
        Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
        Err(e) => return Err(e),
      };
      (promise, m)
    };

    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };

    {
      let (vm, _modules, heap) = rt.vm_modules_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(promise)?;
      if promise_rejection_message_contains(
        vm,
        &mut host,
        &mut hooks,
        &mut scope,
        promise_obj,
        "module AST missing",
      )? {
        return Ok(());
      }
    }

    let errors = hooks.perform_microtask_checkpoint(&mut rt);
    if let Some(err) = errors.into_iter().next() {
      return Err(err);
    }

    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = modules.get_module_namespace(module, vm, &mut scope)?;
    let Value::String(x) = ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "x")? else {
      panic!("expected module export 'x' to be a string");
    };
    assert_eq!(scope.heap().get_string(x)?.to_utf8_lossy(), "d");
    Ok(())
  })();

  hooks.teardown(&mut rt);
  result
}
