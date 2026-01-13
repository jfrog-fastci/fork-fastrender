use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  // Keep heap limits small for test speed but large enough that minor intrinsic layout changes
  // don't cause unrelated OOM failures.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn assert_is_function(v: Value) -> GcObject {
  let Value::Object(o) = v else {
    panic!("expected function object, got {v:?}");
  };
  o
}

use crate::GcObject;

#[test]
fn class_elements_set_function_home_object_ast() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  rt.exec_script(
    r#"
      var A = class {
        constructor() {}
        m() { return 1; }
        static s() { return 2; }
        mArrow() { return () => 3; }
        static sArrow() { return () => 4; }
        x = () => 5;
        static y = () => 6;
      };

      // Keep these function objects alive from the JS side so GC won't collect them between host
      // calls while the test inspects their metadata.
      var instMethod = A.prototype.m;
      var staticMethod = A.s;
      var instArrow = (new A()).mArrow();
      var staticArrow = A.sArrow();
      var instFieldArrow = (new A()).x;
      var staticFieldArrow = A.y;
    "#,
  )?;

  let ctor = assert_is_function(rt.exec_script("A")?);
  let Value::Object(proto) = rt.exec_script("A.prototype")? else {
    panic!("expected A.prototype to be object");
  };

  let inst_method = assert_is_function(rt.exec_script("instMethod")?);
  let static_method = assert_is_function(rt.exec_script("staticMethod")?);
  let inst_arrow = assert_is_function(rt.exec_script("instArrow")?);
  let static_arrow = assert_is_function(rt.exec_script("staticArrow")?);
  let inst_field_arrow = assert_is_function(rt.exec_script("instFieldArrow")?);
  let static_field_arrow = assert_is_function(rt.exec_script("staticFieldArrow")?);

  assert_eq!(rt.heap().get_function_home_object(inst_method)?, Some(proto));
  assert_eq!(rt.heap().get_function_home_object(static_method)?, Some(ctor));
  assert_eq!(rt.heap().get_function_home_object(inst_arrow)?, Some(proto));
  assert_eq!(rt.heap().get_function_home_object(static_arrow)?, Some(ctor));
  assert_eq!(
    rt.heap().get_function_home_object(inst_field_arrow)?,
    Some(proto)
  );
  assert_eq!(
    rt.heap().get_function_home_object(static_field_arrow)?,
    Some(ctor)
  );

  // Ensure the hidden user-defined constructor body function has `[[HomeObject]]` set (needed for
  // `super.prop` inside constructors).
  {
    let mut scope = rt.heap.scope();
    scope.push_roots(&[Value::Object(ctor), Value::Object(proto)])?;
    let Some(body_func) = crate::class_fields::class_constructor_body(&scope, ctor)? else {
      return Err(VmError::InvariantViolation(
        "expected class constructor to have a body function",
      ));
    };
    assert_eq!(
      scope.heap().get_function_home_object(body_func)?,
      Some(proto)
    );

    // Ensure instance-field initializer functions get `[[HomeObject]]` so arrow functions created
    // inside them can resolve `super.prop` lexically.
    let pairs = crate::class_fields::class_constructor_instance_field_pairs(&scope, ctor)?;
    let mut found_x = false;
    for pair in pairs.chunks_exact(2) {
      let key = pair[0];
      let init = pair[1];
      let Value::String(key_s) = key else {
        continue;
      };
      if scope.heap().get_string(key_s)?.to_utf8_lossy() != "x" {
        continue;
      }
      let Value::Object(init_func) = init else {
        return Err(VmError::InvariantViolation(
          "instance field initializer slot is not a function object",
        ));
      };
      found_x = true;
      assert_eq!(
        scope.heap().get_function_home_object(init_func)?,
        Some(proto)
      );
    }
    assert!(found_x, "expected to find instance field initializer for `x`");
  }

  Ok(())
}

#[test]
fn class_elements_set_function_home_object_hir() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Note: compiled-HIR execution does not yet support class fields, so this test only covers
  // methods/accessors and arrow-function creation inside them.
  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "<inline>",
    r#"
      var A = class {
        constructor() {}
        mArrow() { return () => 1; }
        static sArrow() { return () => 2; }
      };

      // Keep these function objects alive from the JS side so GC won't collect them between host
      // calls while the test inspects their metadata.
      var instMethod = A.prototype.mArrow;
      var staticMethod = A.sArrow;
      var instArrow = (new A()).mArrow();
      var staticArrow = A.sArrow();
    "#,
  )?;
  rt.exec_compiled_script(script)?;

  let ctor = assert_is_function(rt.exec_script("A")?);
  let Value::Object(proto) = rt.exec_script("A.prototype")? else {
    panic!("expected A.prototype to be object");
  };

  let inst_method = assert_is_function(rt.exec_script("instMethod")?);
  let static_method = assert_is_function(rt.exec_script("staticMethod")?);
  let inst_arrow = assert_is_function(rt.exec_script("instArrow")?);
  let static_arrow = assert_is_function(rt.exec_script("staticArrow")?);

  assert_eq!(rt.heap().get_function_home_object(inst_method)?, Some(proto));
  assert_eq!(rt.heap().get_function_home_object(static_method)?, Some(ctor));
  assert_eq!(rt.heap().get_function_home_object(inst_arrow)?, Some(proto));
  assert_eq!(rt.heap().get_function_home_object(static_arrow)?, Some(ctor));

  // Ensure the compiled constructor body function also has `[[HomeObject]]` set.
  {
    let mut scope = rt.heap.scope();
    scope.push_roots(&[Value::Object(ctor), Value::Object(proto)])?;

    let Some(wrapper) = crate::class_fields::class_constructor_body(&scope, ctor)? else {
      return Err(VmError::InvariantViolation(
        "expected class constructor to have a body wrapper",
      ));
    };
    let slots = scope.heap().get_function_native_slots(wrapper)?;
    let Some(Value::Object(body_func)) = slots.first().copied() else {
      return Err(VmError::InvariantViolation(
        "compiled constructor body wrapper missing body function slot",
      ));
    };
    assert_eq!(
      scope.heap().get_function_home_object(body_func)?,
      Some(proto)
    );
  }

  Ok(())
}
