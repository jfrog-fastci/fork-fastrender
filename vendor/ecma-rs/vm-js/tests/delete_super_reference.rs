use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Some of these tests use Promises/async-await; give them a slightly larger heap to avoid
  // spurious OOMs.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

fn assert_value_is_number(value: Value, expected: f64) {
  let Value::Number(n) = value else {
    panic!("expected number, got {value:?}");
  };
  assert_eq!(n, expected);
}

#[test]
fn delete_super_property_base_instance_method_throws_reference_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    class C {
      del() {
        try { delete super.m; return "no"; }
        catch (e) { return e.name; }
      }
    }
    new C().del()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn delete_super_property_base_static_method_throws_reference_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    class C {
      static del() {
        try { delete super.m; return "no"; }
        catch (e) { return e.name; }
      }
    }
    C.del()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn delete_super_property_computed_member_throws_reference_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    class C {
      del() {
        try { delete super["m"]; return "no"; }
        catch (e) { return e.name; }
      }
    }
    new C().del()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn delete_super_property_computed_member_evaluates_key_expression() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    class C {
      del() {
        let side = 0;
        try { delete super[(side = 1, "m")]; return "no"; }
        catch (e) { return side; }
      }
    }
    new C().del()
    "#,
  )?;

  assert_value_is_number(value, 1.0);
  Ok(())
}

#[test]
fn delete_super_property_computed_member_propagates_to_property_key_errors() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    class C {
      del() {
        try {
          delete super[{ toString() { throw "x"; } }];
          return "no";
        } catch (e) {
          return e;
        }
      }
    }
    new C().del()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "x");
  Ok(())
}

#[test]
fn delete_super_property_computed_member_with_await_in_key_throws_reference_error() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      class C {
        async del() {
          let side = 0;
          try {
            delete super[await (side = 1, Promise.resolve("m"))];
            return "no";
          } catch (e) {
            return String(side) + ":" + e.name;
          }
        }
      }
      new C().del().then(function (v) { out = v; });
      out
    "#,
  )?;

  // Promise not resolved yet.
  assert_value_is_utf8(&rt, value, "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_value_is_utf8(&rt, value, "1:ReferenceError");
  Ok(())
}

fn exec_or_skip_class_inheritance(rt: &mut JsRuntime, script: &str) -> Result<Option<Value>, VmError> {
  match rt.exec_script(script) {
    Ok(v) => Ok(Some(v)),
    Err(VmError::Unimplemented(msg)) if msg.contains("class inheritance") => Ok(None),
    Err(err) => Err(err),
  }
}

#[test]
fn delete_super_property_derived_instance_method_throws_reference_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      class B { m(){} }
      class D extends B {
        del() {
          try { delete super.m; return "no"; }
          catch (e) { return e.name; }
        }
      }
      new D().del()
    "#,
  )?
  else {
    return Ok(());
  };

  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn delete_super_property_derived_static_method_throws_reference_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      class B { static m(){} }
      class D extends B {
        static del() {
          try { delete super.m; return "no"; }
          catch (e) { return e.name; }
        }
      }
      D.del()
    "#,
  )?
  else {
    return Ok(());
  };

  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}
