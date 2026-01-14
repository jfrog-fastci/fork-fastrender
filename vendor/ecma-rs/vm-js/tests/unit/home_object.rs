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
        mNested() { return () => () => 7; }
        static sNested() { return () => () => 8; }
        x = () => 5;
        static y = () => 6;
        static {
          // Arrow functions created in static blocks capture the class constructor as their
          // `[[HomeObject]]`.
          this.blockArrow = () => 9;
          this.blockNested = () => () => 10;
        }
      };

      // Keep these function objects alive from the JS side so GC won't collect them between host
      // calls while the test inspects their metadata.
      var instMethod = A.prototype.m;
      var staticMethod = A.s;
      var instArrow = (new A()).mArrow();
      var staticArrow = A.sArrow();
      var instNested1 = (new A()).mNested();
      var instNested2 = instNested1();
      var staticNested1 = A.sNested();
      var staticNested2 = staticNested1();
      var instFieldArrow = (new A()).x;
      var staticFieldArrow = A.y;
      var blockArrow = A.blockArrow;
      var blockNested1 = A.blockNested;
      var blockNested2 = A.blockNested();
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
  let inst_nested_1 = assert_is_function(rt.exec_script("instNested1")?);
  let inst_nested_2 = assert_is_function(rt.exec_script("instNested2")?);
  let static_nested_1 = assert_is_function(rt.exec_script("staticNested1")?);
  let static_nested_2 = assert_is_function(rt.exec_script("staticNested2")?);
  let inst_field_arrow = assert_is_function(rt.exec_script("instFieldArrow")?);
  let static_field_arrow = assert_is_function(rt.exec_script("staticFieldArrow")?);
  let block_arrow = assert_is_function(rt.exec_script("blockArrow")?);
  let block_nested_1 = assert_is_function(rt.exec_script("blockNested1")?);
  let block_nested_2 = assert_is_function(rt.exec_script("blockNested2")?);

  assert_eq!(rt.heap().get_function_home_object(inst_method)?, Some(proto));
  assert_eq!(rt.heap().get_function_home_object(static_method)?, Some(ctor));
  assert_eq!(rt.heap().get_function_home_object(inst_arrow)?, Some(proto));
  assert_eq!(rt.heap().get_function_home_object(static_arrow)?, Some(ctor));
  assert_eq!(rt.heap().get_function_home_object(inst_nested_1)?, Some(proto));
  assert_eq!(rt.heap().get_function_home_object(inst_nested_2)?, Some(proto));
  assert_eq!(
    rt.heap().get_function_home_object(static_nested_1)?,
    Some(ctor)
  );
  assert_eq!(
    rt.heap().get_function_home_object(static_nested_2)?,
    Some(ctor)
  );
  assert_eq!(
    rt.heap().get_function_home_object(inst_field_arrow)?,
    Some(proto)
  );
  assert_eq!(
    rt.heap().get_function_home_object(static_field_arrow)?,
    Some(ctor)
  );
  assert_eq!(rt.heap().get_function_home_object(block_arrow)?, Some(ctor));
  assert_eq!(rt.heap().get_function_home_object(block_nested_1)?, Some(ctor));
  assert_eq!(rt.heap().get_function_home_object(block_nested_2)?, Some(ctor));

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
        mNested() { return () => () => 3; }
        static sNested() { return () => () => 4; }
        static {
          this.blockArrow = () => 5;
          this.blockNested = () => () => 6;
        }
      };

      // Keep these function objects alive from the JS side so GC won't collect them between host
      // calls while the test inspects their metadata.
      var instMethod = A.prototype.mArrow;
      var staticMethod = A.sArrow;
      var instArrow = (new A()).mArrow();
      var staticArrow = A.sArrow();
      var instNested1 = (new A()).mNested();
      var instNested2 = instNested1();
      var staticNested1 = A.sNested();
      var staticNested2 = staticNested1();
      var blockArrow = A.blockArrow;
      var blockNested1 = A.blockNested;
      var blockNested2 = A.blockNested();
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
  let inst_nested_1 = assert_is_function(rt.exec_script("instNested1")?);
  let inst_nested_2 = assert_is_function(rt.exec_script("instNested2")?);
  let static_nested_1 = assert_is_function(rt.exec_script("staticNested1")?);
  let static_nested_2 = assert_is_function(rt.exec_script("staticNested2")?);
  let block_arrow = assert_is_function(rt.exec_script("blockArrow")?);
  let block_nested_1 = assert_is_function(rt.exec_script("blockNested1")?);
  let block_nested_2 = assert_is_function(rt.exec_script("blockNested2")?);

  assert_eq!(rt.heap().get_function_home_object(inst_method)?, Some(proto));
  assert_eq!(rt.heap().get_function_home_object(static_method)?, Some(ctor));
  assert_eq!(rt.heap().get_function_home_object(inst_arrow)?, Some(proto));
  assert_eq!(rt.heap().get_function_home_object(static_arrow)?, Some(ctor));
  assert_eq!(rt.heap().get_function_home_object(inst_nested_1)?, Some(proto));
  assert_eq!(rt.heap().get_function_home_object(inst_nested_2)?, Some(proto));
  assert_eq!(
    rt.heap().get_function_home_object(static_nested_1)?,
    Some(ctor)
  );
  assert_eq!(
    rt.heap().get_function_home_object(static_nested_2)?,
    Some(ctor)
  );
  assert_eq!(rt.heap().get_function_home_object(block_arrow)?, Some(ctor));
  assert_eq!(rt.heap().get_function_home_object(block_nested_1)?, Some(ctor));
  assert_eq!(rt.heap().get_function_home_object(block_nested_2)?, Some(ctor));

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

#[test]
fn object_literal_sets_function_home_object_and_inherits_into_arrows() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  rt.exec_script(
    r#"
      var obj = {
        m() { return 1; },
        get x() { return 2; },
        set x(v) {},
        nested() { return () => () => 3; },
      };
      var m = obj.m;
      var g = Object.getOwnPropertyDescriptor(obj, "x").get;
      var s = Object.getOwnPropertyDescriptor(obj, "x").set;
      var nested1 = obj.nested();
      var nested2 = nested1();
    "#,
  )?;

  let Value::Object(obj) = rt.exec_script("obj")? else {
    panic!("expected obj to be an object");
  };
  let m = assert_is_function(rt.exec_script("m")?);
  let g = assert_is_function(rt.exec_script("g")?);
  let s = assert_is_function(rt.exec_script("s")?);
  let nested_1 = assert_is_function(rt.exec_script("nested1")?);
  let nested_2 = assert_is_function(rt.exec_script("nested2")?);

  assert_eq!(rt.heap().get_function_home_object(m)?, Some(obj));
  assert_eq!(rt.heap().get_function_home_object(g)?, Some(obj));
  assert_eq!(rt.heap().get_function_home_object(s)?, Some(obj));
  assert_eq!(rt.heap().get_function_home_object(nested_1)?, Some(obj));
  assert_eq!(rt.heap().get_function_home_object(nested_2)?, Some(obj));

  Ok(())
}

#[test]
fn object_literal_methods_set_function_home_object_async_ast() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Exercise the async evaluator (`async_eval_*`) by running the object literal creation inside an
  // `async` function body. This avoids relying on top-level await parsing in scripts.
  rt.exec_script(
    r#"
      (async function () {
        var o = {
          m() { return 1; },
          mArrow() { return () => 2; },
          get x() { return 3; },
          set x(v) { this._x = v; },
        };

        // Publish these values on the global object so the host test can inspect them.
        globalThis.o = o;
        globalThis.method = o.m;
        globalThis.arrow = o.mArrow();
        var desc = Object.getOwnPropertyDescriptor(o, "x");
        globalThis.getter = desc.get;
        globalThis.setter = desc.set;
      })();
    "#,
  )?;

  let Value::Object(o) = rt.exec_script("o")? else {
    panic!("expected o to be object");
  };

  let method = assert_is_function(rt.exec_script("method")?);
  let arrow = assert_is_function(rt.exec_script("arrow")?);
  let getter = assert_is_function(rt.exec_script("getter")?);
  let setter = assert_is_function(rt.exec_script("setter")?);

  assert_eq!(rt.heap().get_function_home_object(method)?, Some(o));
  assert_eq!(rt.heap().get_function_home_object(getter)?, Some(o));
  assert_eq!(rt.heap().get_function_home_object(setter)?, Some(o));
  assert_eq!(rt.heap().get_function_home_object(arrow)?, Some(o));

  Ok(())
}

#[test]
fn class_static_initialization_sets_home_object_ast() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  rt.exec_script(
    r#"
      var A = class {
        static {
          // Arrow functions created inside static blocks inherit `[[HomeObject]]` from the static
          // block execution context (the class constructor object).
          this.f = () => 1;
        }

        static #x = () => 2;
        static getX() { return this.#x; }
      };

      var staticBlockArrow = A.f;
      var staticPrivateFieldArrow = A.getX();
    "#,
  )?;

  let ctor = assert_is_function(rt.exec_script("A")?);
  let static_block_arrow = assert_is_function(rt.exec_script("staticBlockArrow")?);
  let static_private_field_arrow = assert_is_function(rt.exec_script("staticPrivateFieldArrow")?);

  assert_eq!(
    rt.heap().get_function_home_object(static_block_arrow)?,
    Some(ctor)
  );
  assert_eq!(
    rt.heap().get_function_home_object(static_private_field_arrow)?,
    Some(ctor)
  );

  Ok(())
}

#[test]
fn await_in_class_static_block_runs_as_async_script() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  rt.exec_script(
    r#"
      var out = 0;
      class A {
        static {
          out = 1;
          await Promise.resolve(0);
          out = 2;
        }
      }
    "#,
  )?;

  assert_eq!(rt.exec_script("out")?, Value::Number(1.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.exec_script("out")?, Value::Number(2.0));
  Ok(())
}

#[test]
fn function_home_object_is_traced_by_gc() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  rt.exec_script(
    r#"
      // This object should only remain live through the method function's `[[HomeObject]]`.
      var f = ({ m() { return 1; } }).m;
    "#,
  )?;

  let f = assert_is_function(rt.exec_script("f")?);
  let Some(home_before) = rt.heap().get_function_home_object(f)? else {
    return Err(VmError::InvariantViolation(
      "expected object literal method to have [[HomeObject]]",
    ));
  };
  assert!(rt.heap().is_valid_object(home_before));

  rt.heap.collect_garbage();

  let Some(home_after) = rt.heap().get_function_home_object(f)? else {
    return Err(VmError::InvariantViolation(
      "expected object literal method to still have [[HomeObject]] after GC",
    ));
  };
  assert!(rt.heap().is_valid_object(home_after));

  Ok(())
}
