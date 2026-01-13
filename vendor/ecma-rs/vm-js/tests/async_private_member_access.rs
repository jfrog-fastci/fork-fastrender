use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async/await tends to allocate more than simple synchronous scripts. Use a slightly larger heap
  // than the minimal 1MiB used by some unit tests to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn private_field_read_after_awaited_base() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f(){
        class C { #x = 7; async m(){ return (await Promise.resolve(this)).#x; } }
        return await (new C()).m();
      }
      f().then(v => out = v);
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(7.0));
  Ok(())
}

#[test]
fn private_method_call_after_awaited_base() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f(){
        class C { #m(){ return 9; } async call(){ return (await Promise.resolve(this)).#m(); } }
        return await (new C()).call();
      }
      f().then(v => out = v);
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(9.0));
  Ok(())
}

#[test]
fn private_field_assignment_with_awaited_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f(){
        class C { #x = 1; async set(){ (await Promise.resolve(this)).#x = await Promise.resolve(3); return this.#x; } }
        return await (new C()).set();
      }
      f().then(v => out = v);
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(3.0));
  Ok(())
}

#[test]
fn private_field_add_assign_with_awaited_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f(){
        class C { #x = 1; async add(){ (await Promise.resolve(this)).#x += await Promise.resolve(2); return this.#x; } }
        return await (new C()).add();
      }
      f().then(v => out = v);
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(3.0));
  Ok(())
}

#[test]
fn private_field_update_after_awaited_base() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f(){
        class C { #x = 1; async inc(){ out += ((await Promise.resolve(this)).#x++); out += "," + this.#x; } }
        await (new C()).inc();
      }
      f().then(() => {});
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "1,2");
  Ok(())
}

#[test]
fn proxy_base_should_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f(){
        class C { #x = 1; async m(){ const p = new Proxy(this, {}); return (await Promise.resolve(p)).#x; } }
        return await (new C()).m();
      }
      f().then(v => out = "ok").catch(e => out = e.name);
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "TypeError");
  Ok(())
}

