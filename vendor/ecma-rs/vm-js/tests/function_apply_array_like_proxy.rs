use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value,
  Vm, VmError, VmOptions,
};

fn data_desc(value: Value) -> PropertyDescriptor {
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
  scope.define_property(global, key, data_desc(value))
}

#[test]
fn function_prototype_apply_observes_proxy_get_traps_for_arg_array() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();

    // Target object: { length: 2, 0: "x", 1: "y" }
    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;
    let k0_s = scope.alloc_string("0")?;
    scope.push_root(Value::String(k0_s))?;
    let k1_s = scope.alloc_string("1")?;
    scope.push_root(Value::String(k1_s))?;
    let klen_s = scope.alloc_string("length")?;
    scope.push_root(Value::String(klen_s))?;

    let x_s = scope.alloc_string("x")?;
    scope.push_root(Value::String(x_s))?;
    let y_s = scope.alloc_string("y")?;
    scope.push_root(Value::String(y_s))?;

    scope.define_property(target, PropertyKey::from_string(k0_s), data_desc(Value::String(x_s)))?;
    scope.define_property(target, PropertyKey::from_string(k1_s), data_desc(Value::String(y_s)))?;
    scope.define_property(target, PropertyKey::from_string(klen_s), data_desc(Value::Number(2.0)))?;

    // Handler object (we'll assign `get` from JS).
    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;

    define_global(&mut scope, global, "a", Value::Object(proxy))?;
    define_global(&mut scope, global, "h", Value::Object(handler))?;
  }

  let value = rt.exec_script(
    r#"
      var hits = 0;
      h.get = function (t, p) {
        hits++;
        return t[p];
      };
      (function () { return arguments[0] + arguments[1]; }).apply(null, a) === "xy" && hits > 0
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn function_prototype_apply_invokes_accessors_on_array_like_arg_array() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let value = rt.exec_script(
    r#"
      function f() { return arguments[0]; }
      let a = { get 0() { return "x"; }, get length() { return 1; } };
      f.apply(null, a) === "x"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

