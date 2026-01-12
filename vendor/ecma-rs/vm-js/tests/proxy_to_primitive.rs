use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value,
  Vm, VmError, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn global_var_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn define_global(
  scope: &mut Scope<'_>,
  global: GcObject,
  name: &str,
  value: Value,
) -> Result<(), VmError> {
  scope.push_root(Value::Object(global))?;
  scope.push_root(value)?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.define_property(global, key, global_var_desc(value))
}

fn expect_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value, got {value:?}");
  };
  rt.heap()
    .get_string(s)
    .expect("string handle should be valid")
    .to_utf8_lossy()
}

#[test]
fn to_primitive_gets_symbol_to_primitive_via_proxy_get_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  rt.exec_script(
    r#"
      var log = [];
      var handler = {
        get(t, k, r) {
          log.push(String(k));
          if (k === Symbol.toPrimitive) return () => "x";
        }
      };
      var target = {};
    "#,
  )?;

  let Value::Object(target) = rt.exec_script("target")? else {
    return Err(VmError::InvariantViolation("expected target object"));
  };
  let Value::Object(handler) = rt.exec_script("handler")? else {
    return Err(VmError::InvariantViolation("expected handler object"));
  };

  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let v = rt.exec_script("String(p)")?;
  assert_eq!(expect_string(&rt, v), "x");

  let v = rt.exec_script("log[0]")?;
  assert_eq!(expect_string(&rt, v), "Symbol(Symbol.toPrimitive)");
  Ok(())
}

#[test]
fn ordinary_to_primitive_valueof_lookup_uses_proxy_get_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  rt.exec_script(
    r#"
      var log = [];
      var handler = {
        get(t, k, r) {
          log.push(String(k));
          if (k === "valueOf") return function() { return 42; };
        }
      };
      var target = {};
    "#,
  )?;

  let Value::Object(target) = rt.exec_script("target")? else {
    return Err(VmError::InvariantViolation("expected target object"));
  };
  let Value::Object(handler) = rt.exec_script("handler")? else {
    return Err(VmError::InvariantViolation("expected handler object"));
  };

  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let v = rt.exec_script("Number(p)")?;
  assert_eq!(v, Value::Number(42.0));

  let v = rt.exec_script("log[1]")?;
  assert_eq!(expect_string(&rt, v), "valueOf");
  Ok(())
}

#[test]
fn get_prototype_from_constructor_uses_proxy_get_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  rt.exec_script(
    r#"
      var log = [];
      var proto = { marker: 1 };
      var handler = {
        get(t, k, r) {
          log.push(String(k));
          if (k === "prototype") return proto;
        }
      };
      function C(x) { this.x = x; }
    "#,
  )?;

  let Value::Object(target_ctor) = rt.exec_script("C")? else {
    return Err(VmError::InvariantViolation("expected C function object"));
  };
  let Value::Object(handler) = rt.exec_script("handler")? else {
    return Err(VmError::InvariantViolation("expected handler object"));
  };

  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(target_ctor), Some(handler))?;
    define_global(&mut scope, global, "nt", Value::Object(proxy))?;
  }

  let ok = rt.exec_script(
    r#"
      var o = Reflect.construct(C, [2], nt);
      Object.getPrototypeOf(o) === proto &&
      o.x === 2 &&
      log[0] === "prototype"
    "#,
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

