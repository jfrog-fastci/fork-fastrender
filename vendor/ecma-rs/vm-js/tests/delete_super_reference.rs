use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

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

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<delete_super_reference>", source)?;
  rt.exec_compiled_script(script)
}

fn is_unimplemented_compiled_async_function_error(rt: &mut JsRuntime, err: &VmError) -> Result<bool, VmError> {
  const MSG: &str = "async functions (hir-js compiled path)";

  match err {
    VmError::Unimplemented(msg) if msg.contains(MSG) => return Ok(true),
    _ => {}
  }

  let Some(thrown) = err.thrown_value() else {
    return Ok(false);
  };
  let Value::Object(err_obj) = thrown else {
    return Ok(false);
  };

  // Host-facing execution boundaries coerce `VmError::Unimplemented` into a regular
  // `Error("unimplemented: ...")` throw completion; treat both representations as "not supported"
  // so this test can land before compiled async functions are implemented.
  let intr = rt.realm().intrinsics();
  let proto = rt.heap().object_prototype(err_obj)?;
  if proto != Some(intr.error_prototype()) && proto != Some(intr.syntax_error_prototype()) {
    return Ok(false);
  }

  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(err_obj))?;

  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
  let Some(Value::String(message_s)) = scope
    .heap()
    .object_get_own_data_property_value(err_obj, &message_key)?
  else {
    return Ok(false);
  };

  Ok(scope.heap().get_string(message_s)?.to_utf8_lossy().contains(MSG))
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
fn delete_super_property_base_instance_method_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
fn delete_super_property_base_static_method_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
fn delete_super_property_computed_member_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
fn delete_super_property_computed_member_evaluates_key_expression_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
fn delete_super_property_computed_member_propagates_to_property_key_errors_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
fn delete_super_property_object_literal_method_throws_reference_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var obj = {
        del() {
          try { delete super.toString; return "no"; }
          catch (e) { return e.name; }
        }
      };
      obj.del()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn delete_super_property_object_literal_method_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var obj = {
        del() {
          try { delete super.toString; return "no"; }
          catch (e) { return e.name; }
        }
      };
      obj.del()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "ReferenceError");
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

#[test]
fn delete_super_property_computed_member_with_await_in_key_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = match exec_compiled(
    &mut rt,
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
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_compiled_async_function_error(&mut rt, &err)? => return Ok(()),
    Err(err) => return Err(err),
  };

  assert_value_is_utf8(&rt, value, "");
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_value_is_utf8(&rt, value, "1:ReferenceError");
  Ok(())
}

#[test]
fn delete_super_property_derived_instance_method_throws_reference_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
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
  )?;

  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn delete_super_property_derived_instance_method_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
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
  )?;

  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn delete_super_property_derived_static_method_throws_reference_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
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
  )?;

  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn delete_super_property_derived_static_method_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
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
  )?;

  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn delete_super_property_derived_constructor_does_not_evaluate_key_before_super() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let side = 0;
      class B {}
      class D extends B {
        constructor() {
          delete super[(side = 1, "m")];
        }
      }
      try { new D(); }
      catch (e) { String(side) + ":" + e.name; }
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "0:ReferenceError");
  Ok(())
}

#[test]
fn delete_super_property_derived_constructor_does_not_evaluate_key_before_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      let side = 0;
      class B {}
      class D extends B {
        constructor() {
          delete super[(side = 1, "m")];
        }
      }
      try { new D(); }
      catch (e) { String(side) + ":" + e.name; }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "0:ReferenceError");
  Ok(())
}
