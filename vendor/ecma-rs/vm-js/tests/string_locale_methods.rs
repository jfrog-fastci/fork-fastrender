use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn string_case_and_locale_methods_require_object_coercible() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function isTypeError(thunk) {
        try { thunk(); return false; } catch (e) { return e && e.name === "TypeError"; }
      }

      let ok = true;
      ok = ok && isTypeError(() => String.prototype.toLowerCase.call(null));
      ok = ok && isTypeError(() => String.prototype.toUpperCase.call(undefined));
      ok = ok && isTypeError(() => String.prototype.toLocaleLowerCase.call(null));
      ok = ok && isTypeError(() => String.prototype.toLocaleUpperCase.call(undefined));
      ok = ok && isTypeError(() => String.prototype.localeCompare.call(null, "a"));
      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

