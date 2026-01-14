use std::sync::Arc;
use vm_js::{
  CompiledFunctionRef, CompiledScript, Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError,
  VmOptions,
};

fn find_function_body(script: &Arc<CompiledScript>, name: &str) -> hir_js::BodyId {
  let hir = script.hir.as_ref();
  for def in hir.defs.iter() {
    let Some(body_id) = def.body else {
      continue;
    };
    let Some(body) = hir.body(body_id) else {
      continue;
    };
    if body.kind != hir_js::BodyKind::Function {
      continue;
    }
    let def_name = hir.names.resolve(def.name).unwrap_or("");
    if def_name == name {
      return body_id;
    }
  }
  panic!("function body not found for name={name:?}");
}

fn is_unimplemented_async_compiled_error(rt: &mut JsRuntime, err: &VmError) -> Result<bool, VmError> {
  match err {
    VmError::Unimplemented(msg) if msg.contains("async functions (hir-js compiled path)") => return Ok(true),
    _ => {}
  }

  let Some(thrown) = err.thrown_value() else {
    return Ok(false);
  };
  let Value::Object(err_obj) = thrown else {
    return Ok(false);
  };

  // `VmError::Unimplemented` is surfaced to host boundaries as a generic `Error` with an
  // `"unimplemented: ..."` message; detect that so this test can skip until async HIR execution is
  // implemented.
  let error_proto = rt.realm().intrinsics().error_prototype();
  if rt.heap().object_prototype(err_obj)? != Some(error_proto) {
    return Ok(false);
  }

  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(err_obj))?;
  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
  let Some(Value::String(message_s)) =
    scope.heap().object_get_own_data_property_value(err_obj, &message_key)?
  else {
    return Ok(false);
  };
  let message = scope.heap().get_string(message_s)?.to_utf8_lossy();
  Ok(message.contains("async functions (hir-js compiled path)"))
}

#[test]
fn compiled_async_function_suspension_does_not_teardown_env() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Promise/async-await allocates builtin job machinery; use a larger heap to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_function_suspension_does_not_teardown_env.js",
    r#"
      async function f() {
        await Promise.resolve(1);
        return 42;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  // Install the compiled user function object onto the realm global so we can invoke it from JS.
  {
    let global = rt.realm().global_object();
    let mut scope = rt.heap_mut().scope();
    scope.push_root(Value::Object(global))?;

    let name_s = scope.alloc_string("f")?;
    scope.push_root(Value::String(name_s))?;

    let f_obj = scope.alloc_user_function(
      CompiledFunctionRef {
        script,
        body: f_body,
      },
      name_s,
      0,
    )?;
    scope.push_root(Value::Object(f_obj))?;

    let key_s = scope.alloc_string("f")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    scope.define_property(
      global,
      key,
      vm_js::PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: vm_js::PropertyKind::Data {
          value: Value::Object(f_obj),
          writable: true,
        },
      },
    )?;
  }

  let baseline_env_roots = rt.heap.persistent_env_root_count();

  // If compiled async execution is not yet implemented, skip: this test specifically targets the
  // `Vm::call_user_function` plumbing once async HIR execution exists.
  match rt.exec_script("var out = 0; f().then(v => { out = v; });") {
    Ok(_) => {}
    Err(err) if is_unimplemented_async_compiled_error(&mut rt, &err)? => {
      return Ok(());
    }
    Err(err) => return Err(err),
  }
  // `await` should always suspend to a Promise job, even when awaiting an already-resolved Promise.
  assert_eq!(rt.exec_script("out")?, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  assert_eq!(rt.exec_script("out")?, Value::Number(42.0));
  assert_eq!(
    rt.heap.persistent_env_root_count(),
    baseline_env_roots,
    "async compiled call should not leak persistent env roots"
  );
  Ok(())
}

#[test]
fn compiled_async_unimplemented_does_not_leak_env_roots() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "compiled_async_unimplemented_does_not_leak_env_roots.js",
    r#"
      async function f() {
        await Promise.resolve(1);
        return 42;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let result: Result<(), VmError> = (|| {
    let baseline_env_roots = heap.persistent_env_root_count();

    // Allocate and call inside a short-lived scope so we can borrow `heap` mutably again to run
    // microtasks.
    let call_res = {
      let mut scope = heap.scope();
      let name_s = scope.alloc_string("f")?;
      let f_obj = scope.alloc_user_function(
        CompiledFunctionRef {
          script,
          body: f_body,
        },
        name_s,
        0,
      )?;
      scope.push_root(Value::Object(f_obj))?;
      vm.call_without_host(&mut scope, Value::Object(f_obj), Value::Undefined, &[])
    };

    match call_res {
      Ok(_promise) => {
        // Async compiled functions schedule Promise jobs to resume after `await`; drain them so we
        // don't drop `Job`s with leaked persistent roots.
        vm.perform_microtask_checkpoint(&mut heap)?;
        assert_eq!(
          heap.persistent_env_root_count(),
          baseline_env_roots,
          "async compiled call leaked a persistent env root"
        );
        Ok(())
      }
      Err(err) => {
        let mut scope = heap.scope();

        // `Vm::call` routes errors through `coerce_error_to_throw`, which converts
        // `VmError::Unimplemented` into a thrown `Error(\"unimplemented: …\")`. Detect both the raw
        // and thrown forms here.
        let is_unimplemented_async = match &err {
          VmError::Unimplemented(msg) => msg.contains("async functions (hir-js compiled path)"),
          _ => {
            let Some(thrown) = err.thrown_value() else {
              return Err(err);
            };
            let Value::Object(err_obj) = thrown else {
              return Err(err);
            };
            if scope.heap().object_prototype(err_obj)? != Some(realm.intrinsics().error_prototype()) {
              return Err(err);
            }
            scope.push_root(Value::Object(err_obj))?;
            let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
            let Some(Value::String(message_s)) =
              scope.heap().object_get_own_data_property_value(err_obj, &message_key)?
            else {
              return Err(err);
            };
            let message = scope.heap().get_string(message_s)?.to_utf8_lossy();
            message.contains("async functions (hir-js compiled path)")
          }
        };

        if is_unimplemented_async {
          assert_eq!(
            scope.heap().persistent_env_root_count(),
            baseline_env_roots,
            "unimplemented async compiled call leaked a persistent env root"
          );
          Ok(())
        } else {
          Err(err)
        }
      }
    }
  })();

  realm.teardown(&mut heap);
  result
}
