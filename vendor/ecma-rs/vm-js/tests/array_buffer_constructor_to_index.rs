use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn array_buffer_constructor_uses_to_index() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // `undefined` and `NaN` convert to 0.
  assert_eq!(rt.exec_script("new ArrayBuffer().byteLength")?, Value::Number(0.0));
  assert_eq!(
    rt.exec_script("new ArrayBuffer(undefined).byteLength")?,
    Value::Number(0.0)
  );
  assert_eq!(rt.exec_script("new ArrayBuffer(NaN).byteLength")?, Value::Number(0.0));

  // `ToNumber([1,2])` is `NaN`, so `ToIndex([1,2])` is 0 (no error).
  assert_eq!(
    rt.exec_script("new ArrayBuffer([1,2]).byteLength")?,
    Value::Number(0.0)
  );

  // Fractional lengths are truncated.
  assert_eq!(
    rt.exec_script("new ArrayBuffer(1.9).byteLength")?,
    Value::Number(1.0)
  );

  // Negative and infinite lengths throw RangeError (per `ToIndex`).
  assert_eq!(
    rt.exec_script(
      "(() => { try { new ArrayBuffer(-1); } catch (e) { return e.name === 'RangeError'; } return false; })()"
    )?,
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script(
      "(() => { try { new ArrayBuffer(Infinity); } catch (e) { return e.name === 'RangeError'; } return false; })()"
    )?,
    Value::Bool(true)
  );

  // Values above 2^53 - 1 throw RangeError (ToIndex step 5).
  assert_eq!(
    rt.exec_script(
      "(() => { try { new ArrayBuffer(9007199254740992); } catch (e) { return e.name === 'RangeError'; } return false; })()"
    )?,
    Value::Bool(true)
  );

  Ok(())
}

