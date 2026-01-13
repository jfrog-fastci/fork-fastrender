use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

#[test]
fn generator_private_field_get_with_yield_in_base() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      (function () {
        class C { static #x = 1; static *g(){ return (yield this).#x; } }
        var it = C.g();
        var r1 = it.next();
        var r2 = it.next(r1.value);
        return r1.done === false && r2.done === true && r2.value === 1;
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_private_method_call_with_yield_in_base() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      (function () {
        class C { static #m(){ return 2; } static *g(){ return (yield this).#m(); } }
        var it = C.g();
        var r1 = it.next();
        var r2 = it.next(r1.value);
        return r2.done === true && r2.value === 2;
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_private_instance_field_get_with_yield_in_base() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      (function () {
        class C { #x = 1; *g(){ return (yield this).#x; } }
        var c = new C();
        var it = c.g();
        var r1 = it.next();
        var r2 = it.next(r1.value);
        return r1.done === false && r2.done === true && r2.value === 1;
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_private_instance_method_call_with_yield_in_base() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      (function () {
        class C { #m(){ return 2; } *g(){ return (yield this).#m(); } }
        var c = new C();
        var it = c.g();
        var r1 = it.next();
        var r2 = it.next(r1.value);
        return r2.done === true && r2.value === 2;
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
