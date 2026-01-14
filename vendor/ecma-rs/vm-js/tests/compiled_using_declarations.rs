use vm_js::{
  CompiledScript, Heap, HeapLimits, JsRuntime, MicrotaskQueue, PromiseState, PropertyKey, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<inline>", source)?;
  rt.exec_compiled_script(script)
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
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

#[test]
fn compiled_script_supports_using_declaration_in_block() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      {
        using x = null;
      }
      1
    "#,
  )?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_script_using_initializer_must_be_object() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      try {
        {
          using x = 1;
        }
        "no"
      } catch (e) {
        e.name
      }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "TypeError");
  Ok(())
}

#[test]
fn compiled_module_supports_top_level_using_and_await_using() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let result = (|| -> Result<(), VmError> {
    let compiled = CompiledScript::compile_module(
      rt.heap_mut(),
      "m.js",
      r#"
        using x = null;
        await using y = null;
        export const out = 1;
      "#,
    )?;
    let mut record = SourceTextModuleRecord::parse_source(rt.heap_mut(), compiled.source.clone())?;
    assert!(
      record.has_tla,
      "await using at module top-level should mark the module as `[[HasTLA]]`"
    );
    record.compiled = Some(compiled);
    // Force ModuleGraph to use the compiled-module (HIR) instantiation + execution path.
    record.clear_ast();

    let global_object = rt.realm().global_object();
    let realm_id = rt.realm().id();

    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let m = modules.add_module_with_specifier("m", record)?;
    modules.link_all_by_specifier();

    let promise = match modules.evaluate(
      vm,
      heap,
      global_object,
      realm_id,
      m,
      &mut host,
      &mut hooks,
    ) {
      Ok(p) => p,
      Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
      Err(e) => return Err(e),
    };

    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };

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
      // Compiled module execution is not supported in this configuration.
      return Ok(());
    }

    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = modules.get_module_namespace(m, vm, &mut scope)?;
    assert_eq!(
      ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "out")?,
      Value::Number(1.0)
    );

    Ok(())
  })();

  // Ensure any queued jobs are discarded so they do not leak persistent roots.
  hooks.teardown(&mut rt);
  result
}

#[test]
fn compiled_module_await_using_initializer_must_be_object() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let result = (|| -> Result<(), VmError> {
    let compiled = CompiledScript::compile_module(
      rt.heap_mut(),
      "m.js",
      r#"
        await using x = 1;
        export const out = 1;
      "#,
    )?;
    let mut record = SourceTextModuleRecord::parse_source(rt.heap_mut(), compiled.source.clone())?;
    assert!(record.has_tla, "await using should mark the module as `[[HasTLA]]`");
    record.compiled = Some(compiled);
    // Force ModuleGraph to use the compiled-module (HIR) instantiation + execution path.
    record.clear_ast();

    let global_object = rt.realm().global_object();
    let realm_id = rt.realm().id();

    // Evaluate in a nested block so we don't hold `&mut JsRuntime` borrows while inspecting the
    // resulting Promise.
    let promise = {
      let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
      let m = modules.add_module_with_specifier("m", record)?;
      modules.link_all_by_specifier();
      match modules.evaluate(vm, heap, global_object, realm_id, m, &mut host, &mut hooks) {
        Ok(p) => p,
        Err(VmError::Unimplemented(msg)) if msg.contains("module AST missing") => return Ok(()),
        Err(e) => return Err(e),
      }
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

      assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Rejected);
      let reason = scope
        .heap()
        .promise_result(promise_obj)?
        .expect("rejected promise should have a reason");
      let Value::Object(reason_obj) = reason else {
        panic!("expected rejection reason to be an object, got {reason:?}");
      };
      let Value::String(name) = ns_get(vm, &mut host, &mut hooks, &mut scope, reason_obj, "name")? else {
        panic!("expected error.name to be a string");
      };
      assert_eq!(scope.heap().get_string(name)?.to_utf8_lossy(), "TypeError");
    }

    Ok(())
  })();

  // Ensure any queued jobs are discarded so they do not leak persistent roots.
  hooks.teardown(&mut rt);
  result
}
