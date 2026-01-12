use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn reflect_global_exists() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      typeof Reflect === "object" &&
      Reflect !== null &&
      typeof Reflect.apply === "function" &&
      Reflect.apply.length === 3 &&
      typeof Reflect.construct === "function" &&
      Reflect.construct.length === 2
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn reflect_get_set_receiver_behavior() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var target = {
        get x() { return this.y; },
        set x(v) { this.y = v; },
      };
      var receiver = { y: 1 };
      var ok1 = Reflect.get(target, "x", receiver) === 1;
      var ok2 = Reflect.set(target, "x", 2, receiver) === true && receiver.y === 2;
      var ok3 = Reflect.set(target, "x", 3) === true && target.y === 3;
      ok1 && ok2 && ok3
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn reflect_construct_works() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function C(x) { this.x = x; }
      function D() {}
      D.prototype = { marker: 1 };

      var o1 = Reflect.construct(C, [1]);
      var o2 = Reflect.construct(C, [2], D);

      (o1 instanceof C) &&
      o1.x === 1 &&
      Object.getPrototypeOf(o2) === D.prototype &&
      o2.x === 2
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn reflect_own_keys_order_matches_ordinary_own_property_keys() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var s1 = Symbol("s1");
      var s2 = Symbol("s2");
      var o = {};
      o.b = 1;
      o[1] = 2;
      o.a = 3;
      o[0] = 4;
      o[s1] = 5;
      o[s2] = 6;

      var keys = Reflect.ownKeys(o);
      keys.length === 6 &&
      keys[0] === "0" &&
      keys[1] === "1" &&
      keys[2] === "b" &&
      keys[3] === "a" &&
      keys[4] === s1 &&
      keys[5] === s2
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

