use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm,
  VmError, VmOptions,
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
fn for_of_observes_proxy_get_trap_for_symbol_iterator() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let global = rt.realm().global_object();

  // Create a `get` trap that counts how many times it's called and forwards to the target.
  let trap = rt.exec_script(
    r#"
      globalThis.trapCount = 0;
      (function (target, prop, receiver) {
        globalThis.trapCount++;
        return Reflect.get(target, prop, receiver);
      })
    "#,
  )?;

  // Target is a simple iterable that terminates immediately.
  let target = rt.exec_script(
    r#"
      ({
        [Symbol.iterator]: function () {
          return {
            next: function () { return { done: true }; }
          };
        }
      })
    "#,
  )?;

  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(target)?;
    scope.push_root(trap)?;

    let Value::Object(target_obj) = target else {
      panic!("expected target object, got {target:?}");
    };

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let get_key_s = scope.alloc_string("get")?;
    scope.push_root(Value::String(get_key_s))?;
    let get_key = PropertyKey::from_string(get_key_s);
    scope.define_property(handler, get_key, data_desc(trap))?;

    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler))?;
    define_global(&mut scope, global, "proxyIterable", Value::Object(proxy))?;
  }

  let count = rt.exec_script(
    r#"
      for (const _ of proxyIterable) {}
      trapCount
    "#,
  )?;
  assert_eq!(count, Value::Number(1.0));
  Ok(())
}

#[test]
fn iterator_result_done_and_value_use_proxy_get_trap() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let global = rt.realm().global_object();

  // Create a `get` trap that counts property reads and forwards to the target.
  let trap = rt.exec_script(
    r#"
      globalThis.doneGets = 0;
      globalThis.valueGets = 0;
      (function (target, prop, receiver) {
        if (prop === "done") globalThis.doneGets++;
        if (prop === "value") globalThis.valueGets++;
        return Reflect.get(target, prop, receiver);
      })
    "#,
  )?;

  let iter_result_target = rt.exec_script("({ done: false, value: 123 })")?;

  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(trap)?;
    scope.push_root(iter_result_target)?;

    let Value::Object(target_obj) = iter_result_target else {
      panic!("expected target object, got {iter_result_target:?}");
    };

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let get_key_s = scope.alloc_string("get")?;
    scope.push_root(Value::String(get_key_s))?;
    let get_key = PropertyKey::from_string(get_key_s);
    scope.define_property(handler, get_key, data_desc(trap))?;

    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler))?;
    define_global(&mut scope, global, "iterResultProxy", Value::Object(proxy))?;
  }

  let ok = rt.exec_script(
    r#"
      var i = 0;
      var seen = undefined;
      var iterable = {
        [Symbol.iterator]: function () {
          return {
            next: function () {
              i++;
              if (i === 1) return iterResultProxy;
              return { done: true };
            }
          };
        }
      };

      for (var x of iterable) {
        seen = x;
      }

      seen === 123 && doneGets === 1 && valueGets === 1
    "#,
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn revoked_proxy_iterable_throws_type_error_in_for_of() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let global = rt.realm().global_object();

  let target = rt.exec_script(
    r#"
      ({
        [Symbol.iterator]: function () {
          return { next: function () { return { done: true }; } };
        }
      })
    "#,
  )?;

  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(target)?;

    let Value::Object(target_obj) = target else {
      panic!("expected target object, got {target:?}");
    };

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler))?;
    scope.revoke_proxy(proxy)?;
    define_global(&mut scope, global, "revokedIterable", Value::Object(proxy))?;
  }

  let msg = rt.exec_script(
    r#"
      try {
        for (const _ of revokedIterable) {}
        "no error"
      } catch (e) {
        e.message
      }
    "#,
  )?;
  let msg = expect_string(&rt, msg);
  assert!(
    msg.contains("revoked"),
    "expected revoked-proxy error message, got {msg}"
  );
  Ok(())
}

#[test]
fn for_await_of_observes_proxy_get_trap_for_next_method() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let global = rt.realm().global_object();

  // Create a `get` trap that counts `next` lookups and forwards to the target.
  let trap = rt.exec_script(
    r#"
      globalThis.nextGets = 0;
      (function (target, prop, receiver) {
        if (prop === "next") globalThis.nextGets++;
        return Reflect.get(target, prop, receiver);
      })
    "#,
  )?;

  // Async iterator target: yields one value then completes.
  let target = rt.exec_script(
    r#"
      (() => {
        let i = 0;
        return {
          next: function () {
            i++;
            if (i === 1) return Promise.resolve({ value: "a", done: false });
            return Promise.resolve({ value: undefined, done: true });
          }
        };
      })()
    "#,
  )?;

  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(target)?;
    scope.push_root(trap)?;

    let Value::Object(target_obj) = target else {
      panic!("expected target object, got {target:?}");
    };

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let get_key_s = scope.alloc_string("get")?;
    scope.push_root(Value::String(get_key_s))?;
    let get_key = PropertyKey::from_string(get_key_s);
    scope.define_property(handler, get_key, data_desc(trap))?;

    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler))?;
    define_global(&mut scope, global, "proxyAsyncIterator", Value::Object(proxy))?;
  }

  // Kick off async iteration (resolved via microtasks).
  let out = rt.exec_script(
    r#"
      globalThis.out = "";
      async function f() {
        let log = "";
        const iterable = {
          [Symbol.asyncIterator]: function () {
            return proxyAsyncIterator;
          }
        };
        for await (const x of iterable) {
          log += x;
        }
        return log;
      }
      f().then(function (v) { globalThis.out = v; });
      globalThis.out
    "#,
  )?;
  assert_eq!(expect_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("globalThis.out")?;
  assert_eq!(expect_string(&rt, out), "a");

  let next_gets = rt.exec_script("globalThis.nextGets")?;
  assert_eq!(next_gets, Value::Number(1.0));
  Ok(())
}
