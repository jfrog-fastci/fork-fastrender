use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

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

      async function f() {
        await p;
        out = "ok";
      }
      f();
    "#,
  )?;

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
      async function f(){ throw "boom"; }
      f().catch(r => out = r);
    "#,
  )?;

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
      async function f(){ return "ok"; }
      f().then(v => out = v);
    "#,
  )?;

  let out = exec_compiled(&mut rt, "out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = exec_compiled(&mut rt, "out")?;
  assert_eq!(value_to_string(&rt, out), "ok");
  Ok(())
}
