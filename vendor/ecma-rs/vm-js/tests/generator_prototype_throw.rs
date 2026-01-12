use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

#[test]
fn generator_prototype_throw_on_suspended_start_completes_and_throws() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      (function () {
        function* g() { yield 1; }
        const it = g();
        try {
          it.throw(42);
          return false;
        } catch (e) {
          if (e !== 42) return false;
        }

        const r = it.next();
        return r.done === true && r.value === undefined;
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_prototype_throw_on_completed_generator_rethrows() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      (function () {
        function* g() { yield 1; }
        const it = g();
        it.next();
        it.next(); // complete
        try {
          it.throw(7);
          return false;
        } catch (e) {
          return e === 7;
        }
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_prototype_throw_can_be_caught_inside_generator() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      (function () {
        function* g() {
          try {
            yield 1;
          } catch (e) {
            yield e;
          }
          return 9;
        }
        const it = g();
        const r1 = it.next();
        if (r1.value !== 1 || r1.done !== false) return false;

        const r2 = it.throw(5);
        if (r2.value !== 5 || r2.done !== false) return false;

        const r3 = it.next();
        return r3.value === 9 && r3.done === true;
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_prototype_next_reentrancy_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      (function () {
        let it;
        function* g() {
          try {
            it.next();
          } catch (e) {
            return Object.getPrototypeOf(e) === TypeError.prototype;
          }
          return false;
        }
        it = g();
        const r = it.next();
        return r.done === true && r.value === true;
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_prototype_next_rejects_forged_continuation_ids() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      (function () {
        const state = Symbol.for("vm-js.internal.GeneratorState");
        const cont = Symbol.for("vm-js.internal.GeneratorContinuationId");
        const fake = { [state]: 0, [cont]: 999999 };
        const genProto = Object.getPrototypeOf(function* () {}).prototype;
        try {
          genProto.next.call(fake);
          return false;
        } catch (e) {
          return Object.getPrototypeOf(e) === TypeError.prototype;
        }
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
