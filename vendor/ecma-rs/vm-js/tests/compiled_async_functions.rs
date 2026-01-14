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

  rt.exec_script("var out = 0; f().then(v => { out = v; });")?;
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
fn compiled_async_return_await() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_return_await.js",
    r#"
      async function f() {
        return await Promise.resolve(7);
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

  rt.exec_script("var out = 0; f().then(v => { out = v; });")?;
  assert_eq!(rt.exec_script("out")?, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  assert_eq!(rt.exec_script("out")?, Value::Number(7.0));
  assert_eq!(
    rt.heap.persistent_env_root_count(),
    baseline_env_roots,
    "async compiled return-await call should not leak persistent env roots"
  );
  Ok(())
}

#[test]
fn compiled_async_throw_await() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_throw_await.js",
    r#"
      async function f() {
        throw await Promise.resolve('boom');
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

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

  rt.exec_script("var out = 'init'; f().catch(e => { out = e; });")?;
  assert_eq!(rt.exec_script("out === 'init'")?, Value::Bool(true));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  assert_eq!(rt.exec_script("out === 'boom'")?, Value::Bool(true));
  assert_eq!(
    rt.heap.persistent_env_root_count(),
    baseline_env_roots,
    "async compiled throw-await call should not leak persistent env roots"
  );
  Ok(())
}

#[test]
fn compiled_async_var_decl_await() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_var_decl_await.js",
    r#"
      async function f() {
        const x = await Promise.resolve(40), y = await Promise.resolve(2);
        return x + y;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

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

  rt.exec_script("var out = 0; f().then(v => { out = v; });")?;
  assert_eq!(rt.exec_script("out")?, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  assert_eq!(rt.exec_script("out")?, Value::Number(42.0));
  assert_eq!(
    rt.heap.persistent_env_root_count(),
    baseline_env_roots,
    "async compiled var-decl await call should not leak persistent env roots"
  );
  Ok(())
}
