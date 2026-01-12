use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm, VmError,
  VmOptions,
};

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

fn get_global_data_property(
  scope: &mut Scope<'_>,
  global: GcObject,
  name: &str,
) -> Result<Value, VmError> {
  scope.push_root(Value::Object(global))?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(global, &key)?
      .unwrap_or(Value::Undefined),
  )
}

#[test]
fn error_prototype_to_string_is_proxy_get_trap_aware() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  // Build the `target` / `handler` / `log` objects in JS so the trap can capture `log` in a closure.
  rt.exec_script(
    r#"
      globalThis.log = [];
      globalThis.target = { name: "E", message: "m" };
      globalThis.handler = {
        get: function(t, k, r) {
          log.push(String(k));
          return Reflect.get(t, k, r);
        }
      };
    "#,
  )?;

  // Create the Proxy in Rust (vm-js does not expose a JS-level `Proxy` constructor yet).
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();

    let target = get_global_data_property(&mut scope, global, "target")?;
    let handler = get_global_data_property(&mut scope, global, "handler")?;
    let (Value::Object(target), Value::Object(handler)) = (target, handler) else {
      return Err(VmError::Unimplemented("expected target/handler to be objects"));
    };

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let ok = rt.exec_script(
    r#"
      Error.prototype.toString.call(p);
       log.length === 2 && log[0] === "name" && log[1] === "message"
     "#,
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn error_prototype_to_string_invokes_accessors_for_name_and_message() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let ok = rt.exec_script(
    r#"
      var calls = [];
      var o = {};
      Object.defineProperty(o, "name", { get() { calls.push("name"); return "E"; } });
      Object.defineProperty(o, "message", { get() { calls.push("message"); return "m"; } });
      Error.prototype.toString.call(o);
      calls.length === 2 && calls[0] === "name" && calls[1] === "message"
     "#,
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn error_prototype_to_string_is_proxy_aware_in_prototype_chain() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  rt.exec_script(
    r#"
      globalThis.log = [];
      globalThis.protoTarget = { name: "E", message: "m" };
      globalThis.protoHandler = {
        get: function(t, k, r) {
          log.push(String(k));
          return Reflect.get(t, k, r);
        }
      };
    "#,
  )?;

  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();

    let target = get_global_data_property(&mut scope, global, "protoTarget")?;
    let handler = get_global_data_property(&mut scope, global, "protoHandler")?;
    let (Value::Object(target), Value::Object(handler)) = (target, handler) else {
      return Err(VmError::Unimplemented(
        "expected protoTarget/protoHandler to be objects",
      ));
    };

    let proto_proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    // Create `o` with `[[Prototype]] = proto_proxy` without going through `Object.setPrototypeOf`.
    let o = scope.alloc_object_with_prototype(Some(proto_proxy))?;
    define_global(&mut scope, global, "o", Value::Object(o))?;
  }

  let ok = rt.exec_script(
    r#"
      Error.prototype.toString.call(o);
       log.length === 2 && log[0] === "name" && log[1] === "message"
     "#,
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn error_prototype_to_string_throws_on_revoked_proxy() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;
    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    scope.revoke_proxy(proxy)?;
    define_global(&mut scope, global, "revoked", Value::Object(proxy))?;
  }

  let ok = rt.exec_script(
    r#"
      var ok = false;
      try { Error.prototype.toString.call(revoked); } catch (e) { ok = e.name === "TypeError"; }
       ok
     "#,
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}
