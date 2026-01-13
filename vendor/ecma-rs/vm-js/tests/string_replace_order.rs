use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn replace_value_to_string_evaluated_even_when_no_match() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var calls = 0;
      "".replace("a", { toString() { calls++; return "b"; } });
      calls
    "#,
  )?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn replace_value_regexp_object_to_string_evaluated_even_when_no_match() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var calls = 0;
      var re = /b/;
      re.toString = function () { calls++; return "b"; };
      "".replace("a", re);
      calls
    "#,
  )?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn replace_throws_on_null_or_undefined_receiver() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var ok1 = false;
      var ok2 = false;
      try { String.prototype.replace.call(null, "a", "b"); }
      catch (e) { ok1 = e instanceof TypeError; }

      try { String.prototype.replace.call(undefined, "a", "b"); }
      catch (e) { ok2 = e instanceof TypeError; }

      ok1 && ok2
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

