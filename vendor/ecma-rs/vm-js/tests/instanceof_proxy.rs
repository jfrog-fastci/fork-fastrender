use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, RootId, Scope,
  Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // `instanceof` on Proxies can allocate intermediate objects/functions (Proxy traps can synthesize
  // fresh values) and performs prototype-chain walks that may temporarily root multiple objects.
  // Keep the heap limit comfortably above the engine's intrinsic initialization footprint so these
  // tests exercise semantics rather than OOM thresholds.
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
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
  // Root `global`/`value` across string/key allocation in case it triggers GC.
  scope.push_root(Value::Object(global))?;
  scope.push_root(value)?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.define_property(global, key, global_var_desc(value))
}

fn assert_throws_type_error(rt: &mut JsRuntime, script: &str) {
  let err = rt.exec_script(script).unwrap_err();

  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));

  // Root the thrown value across any subsequent allocations / script runs.
  let root: RootId = rt.heap_mut().add_root(thrown).expect("root thrown value");

  let Value::Object(thrown_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };

  let type_error_proto = rt
    .exec_script("globalThis.TypeError.prototype")
    .expect("evaluate TypeError.prototype");
  let Value::Object(type_error_proto) = type_error_proto else {
    panic!("expected TypeError.prototype to be an object");
  };

  let thrown_proto = rt
    .heap()
    .object_prototype(thrown_obj)
    .expect("get thrown prototype");
  assert_eq!(thrown_proto, Some(type_error_proto));

  rt.heap_mut().remove_root(root);
}

fn native_noop_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Undefined)
}

#[test]
fn instanceof_uses_proxy_get_trap_for_symbol_has_instance() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let call_id = vm.register_native_call(native_noop_call)?;

    let mut scope = heap.scope();

    let target_name = scope.alloc_string("target")?;
    scope.push_root(Value::String(target_name))?;
    let target = scope.alloc_native_function(call_id, None, target_name, 0)?;
    scope.push_root(Value::Object(target))?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "rhsProxy", Value::Object(proxy))?;
    define_global(&mut scope, global, "rhsHandler", Value::Object(handler))?;
  }

  let value = rt.exec_script(
    r#"
      var seen = false;
      rhsHandler.get = function(target, prop, receiver) {
        if (prop === Symbol.hasInstance) {
          seen = true;
          return function(v) { return true; };
        }
        return undefined;
      };

      ({} instanceof rhsProxy) === true && seen === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn instanceof_uses_proxy_get_and_getprototypeof_traps() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let call_id = vm.register_native_call(native_noop_call)?;

    let mut scope = heap.scope();

    // RHS: callable Proxy whose `get` trap provides a synthetic `.prototype` object.
    let rhs_target_name = scope.alloc_string("rhsTarget")?;
    scope.push_root(Value::String(rhs_target_name))?;
    let rhs_target = scope.alloc_native_function(call_id, None, rhs_target_name, 0)?;
    scope.push_root(Value::Object(rhs_target))?;

    let rhs_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(rhs_handler))?;
    let rhs_proxy = scope.alloc_proxy(Some(rhs_target), Some(rhs_handler))?;

    define_global(&mut scope, global, "rhsProxy", Value::Object(rhs_proxy))?;
    define_global(&mut scope, global, "rhsHandler", Value::Object(rhs_handler))?;

    // LHS: Proxy whose `getPrototypeOf` trap returns the synthetic RHS prototype.
    let lhs_target = scope.alloc_object()?;
    scope.push_root(Value::Object(lhs_target))?;

    let lhs_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(lhs_handler))?;
    let lhs_proxy = scope.alloc_proxy(Some(lhs_target), Some(lhs_handler))?;

    define_global(&mut scope, global, "lhsProxy", Value::Object(lhs_proxy))?;
    define_global(&mut scope, global, "lhsHandler", Value::Object(lhs_handler))?;
  }

  let value = rt.exec_script(
    r#"
      var seenPrototypeGet = false;
      var seenGetPrototypeOf = false;

      // Synthetic prototype object returned from traps.
      var proto = {};

      rhsHandler.get = function(target, prop, receiver) {
        // Ensure `instanceof` falls back to OrdinaryHasInstance.
        if (prop === Symbol.hasInstance) return undefined;
        if (prop === "prototype") {
          seenPrototypeGet = true;
          return proto;
        }
        return undefined;
      };

      lhsHandler.getPrototypeOf = function(target) {
        seenGetPrototypeOf = true;
        return proto;
      };

      (lhsProxy instanceof rhsProxy) === true && seenPrototypeGet && seenGetPrototypeOf
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn instanceof_uses_function_prototype_has_instance_via_proxy_get_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let call_id = vm.register_native_call(native_noop_call)?;

    let mut scope = heap.scope();

    // RHS: callable Proxy. Its `get` trap forwards `@@hasInstance` to the underlying function so
    // `instanceof` uses `Function.prototype[@@hasInstance]`, but traps `"prototype"`.
    let rhs_target_name = scope.alloc_string("rhsTarget")?;
    scope.push_root(Value::String(rhs_target_name))?;
    let rhs_target = scope.alloc_native_function(call_id, None, rhs_target_name, 0)?;
    scope.push_root(Value::Object(rhs_target))?;

    let rhs_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(rhs_handler))?;
    let rhs_proxy = scope.alloc_proxy(Some(rhs_target), Some(rhs_handler))?;

    define_global(&mut scope, global, "rhsProxy", Value::Object(rhs_proxy))?;
    define_global(&mut scope, global, "rhsHandler", Value::Object(rhs_handler))?;

    // LHS: Proxy whose `getPrototypeOf` trap returns the synthetic RHS prototype.
    let lhs_target = scope.alloc_object()?;
    scope.push_root(Value::Object(lhs_target))?;
    let lhs_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(lhs_handler))?;
    let lhs_proxy = scope.alloc_proxy(Some(lhs_target), Some(lhs_handler))?;

    define_global(&mut scope, global, "lhsProxy", Value::Object(lhs_proxy))?;
    define_global(&mut scope, global, "lhsHandler", Value::Object(lhs_handler))?;
  }

  let value = rt.exec_script(
    r#"
      var seenHasInstanceGet = false;
      var seenPrototypeGet = false;
      var seenGetPrototypeOf = false;

      // Synthetic prototype object returned from traps.
      var proto = {};

      rhsHandler.get = function(target, prop, receiver) {
        if (prop === Symbol.hasInstance) {
          seenHasInstanceGet = true;
          // Forward to the target's ordinary property lookup so we get
          // `Function.prototype[Symbol.hasInstance]`.
          return target[prop];
        }
        if (prop === "prototype") {
          seenPrototypeGet = true;
          return proto;
        }
        return target[prop];
      };

      lhsHandler.getPrototypeOf = function(target) {
        seenGetPrototypeOf = true;
        return proto;
      };

      (lhsProxy instanceof rhsProxy) === true &&
        seenHasInstanceGet &&
        seenPrototypeGet &&
        seenGetPrototypeOf
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn instanceof_bound_function_delegates_to_bound_target_has_instance() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var seen = false;
      function f() {}

      var p = new Proxy(f, {
        get(target, prop, receiver) {
          // This should be observed during `({} instanceof b)` via the OrdinaryHasInstance bound
          // function delegation path.
          if (prop === Symbol.hasInstance) {
            seen = true;
            return function(v) { return true; };
          }
          return target[prop];
        }
      });

      var b = p.bind(null);

      ({} instanceof b) === true && seen === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn instanceof_bound_function_with_undefined_has_instance_delegates_to_bound_target() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var seen = false;
      function f() {}

      var p = new Proxy(f, {
        get(target, prop, receiver) {
          if (prop === Symbol.hasInstance) {
            seen = true;
            return function(v) { return true; };
          }
          return target[prop];
        }
      });

      var b = p.bind(null);

      // Shadow the inherited (non-writable) Function.prototype[@@hasInstance] with an own property
      // so `GetMethod(b, @@hasInstance)` returns `None` and `instanceof` falls back to
      // `OrdinaryHasInstance`, which must delegate to `InstanceofOperator(O, b.[[BoundTargetFunction]])`.
      Object.defineProperty(b, Symbol.hasInstance, { value: undefined });

      (1 instanceof b) === true && ({} instanceof b) === true && seen === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn instanceof_throws_type_error_on_revoked_proxy_lhs_or_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let call_id = vm.register_native_call(native_noop_call)?;

    let mut scope = heap.scope();

    let rhs_target_name = scope.alloc_string("revokedRhsTarget")?;
    scope.push_root(Value::String(rhs_target_name))?;
    let rhs_target = scope.alloc_native_function(call_id, None, rhs_target_name, 0)?;
    scope.push_root(Value::Object(rhs_target))?;

    let rhs_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(rhs_handler))?;
    let revoked_rhs_proxy = scope.alloc_proxy(Some(rhs_target), Some(rhs_handler))?;
    scope.revoke_proxy(revoked_rhs_proxy)?;
    define_global(
      &mut scope,
      global,
      "revokedRhsProxy",
      Value::Object(revoked_rhs_proxy),
    )?;

    let lhs_target = scope.alloc_object()?;
    scope.push_root(Value::Object(lhs_target))?;
    let lhs_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(lhs_handler))?;
    let revoked_lhs_proxy = scope.alloc_proxy(Some(lhs_target), Some(lhs_handler))?;
    scope.revoke_proxy(revoked_lhs_proxy)?;
    define_global(
      &mut scope,
      global,
      "revokedLhsProxy",
      Value::Object(revoked_lhs_proxy),
    )?;
  }

  assert_throws_type_error(&mut rt, r#"({} instanceof revokedRhsProxy)"#);
  assert_throws_type_error(&mut rt, r#"(revokedLhsProxy instanceof Object)"#);

  Ok(())
}
