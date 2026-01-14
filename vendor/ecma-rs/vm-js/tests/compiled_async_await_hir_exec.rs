use std::sync::Arc;
use vm_js::{
  CompiledFunctionRef, CompiledScript, Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions,
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

fn install_compiled_function(rt: &mut JsRuntime, func: CompiledFunctionRef, name: &str) -> Result<(), VmError> {
  let global = rt.realm().global_object();
  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(global))?;

  let name_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(name_s))?;

  let f_obj = scope.alloc_user_function(func, name_s, 0)?;
  scope.push_root(Value::Object(f_obj))?;

  let key_s = scope.alloc_string(name)?;
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
  Ok(())
}

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", source)?;
  rt.exec_compiled_script(script)
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn compiled_await_does_not_invoke_species_constructor() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  exec_compiled(
    &mut rt,
    r#"
      var called = 0;
      var out = "";

      var p = Promise.resolve(1);
      var ctor = {};
      ctor[Symbol.species] = function C(executor) {
        called++;
        return new Promise(executor);
      };
      p.constructor = ctor;

       // Install the async function via `alloc_user_function` in Rust so calling it exercises the
       // compiled (HIR) async function executor (not call-time AST fallback for compiled script
       // function declarations).
     "#,
  )?;

  let f_script = CompiledScript::compile_script(
    &mut rt.heap,
    "<inline>",
    r#"
      async function f() {
        await p;
        out = "ok";
      }
    "#,
  )?;
  let f_body = find_function_body(&f_script, "f");
  install_compiled_function(
    &mut rt,
    CompiledFunctionRef {
      script: f_script,
      body: f_body,
    },
    "f",
  )?;
  exec_compiled(&mut rt, "f();")?;

  assert_eq!(exec_compiled(&mut rt, "called")?, Value::Number(0.0));
  let out = exec_compiled(&mut rt, "out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(exec_compiled(&mut rt, "called")?, Value::Number(0.0));
  let out = exec_compiled(&mut rt, "out")?;
  assert_eq!(value_to_string(&rt, out), "ok");
  Ok(())
}

#[test]
fn compiled_async_throw_rejects_promise_instead_of_throwing_synchronously() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // `throw` inside an `async` function rejects the returned Promise; it must not throw synchronously
  // to the caller.
  exec_compiled(
    &mut rt,
    r#"
      var out = "";
    "#,
  )?;

  let f_script = CompiledScript::compile_script(&mut rt.heap, "<inline>", r#"async function f(){ throw "boom"; }"#)?;
  let f_body = find_function_body(&f_script, "f");
  install_compiled_function(
    &mut rt,
    CompiledFunctionRef {
      script: f_script,
      body: f_body,
    },
    "f",
  )?;

  exec_compiled(&mut rt, "f().catch(r => out = r);")?;

  let out = exec_compiled(&mut rt, "out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = exec_compiled(&mut rt, "out")?;
  assert_eq!(value_to_string(&rt, out), "boom");
  Ok(())
}

#[test]
fn compiled_async_return_resolves_promise() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  exec_compiled(
    &mut rt,
    r#"
      var out = "";
    "#,
  )?;

  let f_script = CompiledScript::compile_script(&mut rt.heap, "<inline>", r#"async function f(){ return "ok"; }"#)?;
  let f_body = find_function_body(&f_script, "f");
  install_compiled_function(
    &mut rt,
    CompiledFunctionRef {
      script: f_script,
      body: f_body,
    },
    "f",
  )?;

  exec_compiled(&mut rt, "f().then(v => out = v);")?;

  let out = exec_compiled(&mut rt, "out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = exec_compiled(&mut rt, "out")?;
  assert_eq!(value_to_string(&rt, out), "ok");
  Ok(())
}

#[test]
fn compiled_async_assignment_await_suspends_and_resumes() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  exec_compiled(
    &mut rt,
    r#"
      var x = "";
      var out = "";
    "#,
  )?;

  let f_script = CompiledScript::compile_script(
    &mut rt.heap,
    "<inline>",
    r#"
      async function f() {
        x = await Promise.resolve("ok");
        return x;
      }
    "#,
  )?;
  let f_body = find_function_body(&f_script, "f");
  install_compiled_function(
    &mut rt,
    CompiledFunctionRef {
      script: f_script,
      body: f_body,
    },
    "f",
  )?;

  exec_compiled(&mut rt, "f().then(v => out = v);")?;
  let x = exec_compiled(&mut rt, "x")?;
  assert_eq!(value_to_string(&rt, x), "");
  let out = exec_compiled(&mut rt, "out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let x = exec_compiled(&mut rt, "x")?;
  assert_eq!(value_to_string(&rt, x), "ok");
  let out = exec_compiled(&mut rt, "out")?;
  assert_eq!(value_to_string(&rt, out), "ok");
  Ok(())
}

#[test]
fn compiled_async_compound_assignment_reads_lhs_before_await() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  exec_compiled(
    &mut rt,
    r#"
      var x = 1;
      var out = 0;
      var p = Promise.resolve().then(() => { x = 100; return 2; });
    "#,
  )?;

  let f_script = CompiledScript::compile_script(
    &mut rt.heap,
    "<inline>",
    r#"
      async function f() {
        x += await p;
        return x;
      }
    "#,
  )?;
  let f_body = find_function_body(&f_script, "f");
  install_compiled_function(
    &mut rt,
    CompiledFunctionRef {
      script: f_script,
      body: f_body,
    },
    "f",
  )?;

  exec_compiled(&mut rt, "f().then(v => out = v);")?;
  assert_eq!(exec_compiled(&mut rt, "x")?, Value::Number(1.0));
  assert_eq!(exec_compiled(&mut rt, "out")?, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(exec_compiled(&mut rt, "x")?, Value::Number(3.0));
  assert_eq!(exec_compiled(&mut rt, "out")?, Value::Number(3.0));
  Ok(())
}
