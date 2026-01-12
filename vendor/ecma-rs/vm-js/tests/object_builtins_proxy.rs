use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn object_get_prototype_of_invokes_proxy_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var p = new Proxy({}, {
        getPrototypeOf: function () { return null; }
      });
      Object.getPrototypeOf(p) === null
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_keys_invokes_proxy_traps() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var calledOwnKeys = false;
      var calledGOPD = false;
      var p = new Proxy({ a: 1 }, {
        ownKeys: function (t) {
          calledOwnKeys = true;
          return ["a"];
        },
        getOwnPropertyDescriptor: function (t, k) {
          calledGOPD = true;
          return { value: 1, enumerable: true, configurable: true, writable: true };
        }
      });
      var keys = Object.keys(p);
      calledOwnKeys && calledGOPD && keys.length === 1 && keys[0] === "a"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_define_property_invokes_proxy_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var called = false;
      var target = {};
      var p = new Proxy(target, {
        defineProperty: function (t, k, desc) {
          called = true;
          return Reflect.defineProperty(t, k, desc);
        }
      });
      Object.defineProperty(p, "x", { value: 1 });
      called && target.x === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_assign_is_proxy_aware() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var calledOwnKeys = false;
      var calledGOPD = false;
      var calledGet = false;

      var source = new Proxy({ a: 1 }, {
        ownKeys: function (t) {
          calledOwnKeys = true;
          return ["a"];
        },
        getOwnPropertyDescriptor: function (t, k) {
          calledGOPD = true;
          return { value: 1, enumerable: true, configurable: true, writable: true };
        },
        get: function (t, k, r) {
          calledGet = true;
          return t[k];
        }
      });

      var target = {};
      var out = Object.assign(target, source);
      calledOwnKeys && calledGOPD && calledGet && out === target && target.a === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
