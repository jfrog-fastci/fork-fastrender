//! Minimal IndexedDB helpers for the `vm-js` Window realm.
//!
//! FastRender's IndexedDB implementation is intentionally incremental. Many real-world libraries
//! assume an `IDBRequest`-style event shape where handlers read `event.target.result` (or ignore the
//! event and read `request.result` directly).
//!
//! This module provides a small reusable event dispatcher that creates a browser-ish event object:
//! `{ type, target, currentTarget, ...extraProps }`, and delivers it to:
//! 1. the `on${type}` attribute handler, and
//! 2. listeners registered via `addEventListener(type, cb)`.
//!
//! Dispatch is best-effort: exceptions thrown by event handlers are swallowed so a single broken
//! callback does not crash the host process.

use vm_js::{
  GcObject, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks,
};

/// Internal listener registry key used by IndexedDB platform objects.
///
/// The storage shape matches the minimal `XMLHttpRequest` listener registry:
///
/// ```text
/// target[LISTENERS_KEY] = {
///   success: [fn, fn, ...],
///   error: [fn, ...],
///   upgradeneeded: [fn, ...],
/// }
/// ```
pub(crate) const LISTENERS_KEY: &str = "__fastrender_idb_listeners";

fn data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn set_data_prop(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
  writable: bool,
) -> Result<(), VmError> {
  // Root `obj` and `value` while allocating the property key: `alloc_key` can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(value, writable))
}

fn get_data_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<Value, VmError> {
  let key = alloc_key(scope, name)?;
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)?
      .unwrap_or(Value::Undefined),
  )
}

fn array_length(scope: &mut Scope<'_>, obj: GcObject) -> Result<usize, VmError> {
  let len_key = alloc_key(scope, "length")?;
  match scope.heap().object_get_own_data_property_value(obj, &len_key)? {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => Ok(n as usize),
    _ => Ok(0),
  }
}

fn call_if_callable(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: Value,
  this: Value,
  args: &[Value],
) {
  if !scope.heap().is_callable(callee).unwrap_or(false) {
    return;
  }
  // Best-effort: user code exceptions should not abort host dispatch.
  let _ = vm.call_with_host_and_hooks(host, scope, hooks, callee, this, args);
}

/// Dispatch an IndexedDB event to `target`.
///
/// The created event object is a plain JS object with:
/// - `type: string`
/// - `target: target`
/// - `currentTarget: target`
/// - `...extra_props` (e.g. `oldVersion`, `newVersion` for `"upgradeneeded"`)
///
/// Dispatch order matches common browser behavior:
/// 1) `target["on" + type]` attribute handler
/// 2) listeners registered via `addEventListener(type, cb)` stored in [`LISTENERS_KEY`]
pub(crate) fn dispatch_idb_event(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  target_obj: GcObject,
  event_type: &str,
  extra_props: &[(&str, Value)],
) -> Result<(), VmError> {
  // Root target while dispatching: handler invocation can allocate/GC.
  scope.push_root(Value::Object(target_obj))?;

  // Build a minimal event object.
  let event_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(event_obj))?;

  let type_s = scope.alloc_string(event_type)?;
  scope.push_root(Value::String(type_s))?;
  set_data_prop(scope, event_obj, "type", Value::String(type_s), true)?;
  set_data_prop(scope, event_obj, "target", Value::Object(target_obj), true)?;
  set_data_prop(
    scope,
    event_obj,
    "currentTarget",
    Value::Object(target_obj),
    true,
  )?;

  for (name, value) in extra_props {
    set_data_prop(scope, event_obj, name, *value, true)?;
  }

  // 1) Event handler property (`onsuccess`, `onerror`, ...).
  // Construct `"on" + type` dynamically (mirrors platform behavior for unknown types).
  let mut handler_prop = String::with_capacity("on".len().saturating_add(event_type.len()));
  handler_prop.push_str("on");
  handler_prop.push_str(event_type);
  let handler = match alloc_key(scope, &handler_prop).and_then(|key| {
    vm.get_with_host_and_hooks(host, scope, hooks, target_obj, key)
  }) {
    Ok(v) => v,
    Err(_) => Value::Undefined,
  };
  call_if_callable(
    vm,
    scope,
    host,
    hooks,
    handler,
    Value::Object(target_obj),
    &[Value::Object(event_obj)],
  );

  // 2) Listener list from `addEventListener`.
  let listeners_val = get_data_prop(scope, target_obj, LISTENERS_KEY)?;
  let Value::Object(listeners_obj) = listeners_val else {
    return Ok(());
  };
  scope.push_root(Value::Object(listeners_obj))?;

  let key = alloc_key(scope, event_type)?;
  let Some(Value::Object(arr)) = scope
    .heap()
    .object_get_own_data_property_value(listeners_obj, &key)?
  else {
    return Ok(());
  };
  scope.push_root(Value::Object(arr))?;

  let len = array_length(scope, arr)?;
  for idx in 0..len {
    let k = alloc_key(scope, &idx.to_string())?;
    let listener = scope
      .heap()
      .object_get_own_data_property_value(arr, &k)?
      .unwrap_or(Value::Undefined);
    call_if_callable(
      vm,
      scope,
      host,
      hooks,
      listener,
      Value::Object(target_obj),
      &[Value::Object(event_obj)],
    );
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use vm_js::Heap;

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value, got {value:?}");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  struct MicrotaskQueueHooks {
    microtasks: *mut vm_js::MicrotaskQueue,
  }

  impl VmHostHooks for MicrotaskQueueHooks {
    fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
      // SAFETY: `microtasks` points into the same VM used for dispatch and outlives this hook
      // (stack-scoped to a single dispatch call).
      unsafe { (&mut *self.microtasks).enqueue_promise_job(job, realm) };
    }
  }

  #[test]
  fn idb_dispatch_event_sets_target_and_result() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    realm.exec_script(
      "globalThis.__ok = false;\n\
       globalThis.req = {\n\
         result: 42,\n\
         onsuccess: function (e) {\n\
           globalThis.__ok = (\n\
             e &&\n\
             e.type === 'success' &&\n\
             e.target === globalThis.req &&\n\
             e.currentTarget === globalThis.req &&\n\
             e.target.result === 42\n\
           );\n\
         }\n\
       };",
    )?;

    let Value::Object(req_obj) = realm.exec_script("req")? else {
      panic!("expected request to be an object");
    };

    {
      let (vm, _realm, heap) = realm.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(req_obj))?;
      let microtasks_ptr = vm.microtask_queue_mut() as *mut vm_js::MicrotaskQueue;
      let mut hooks = MicrotaskQueueHooks {
        microtasks: microtasks_ptr,
      };
      let mut host = ();
      dispatch_idb_event(
        vm,
        &mut scope,
        &mut host,
        &mut hooks,
        req_obj,
        "success",
        &[],
      )?;
    }

    assert_eq!(realm.exec_script("__ok")?, Value::Bool(true));
    realm.teardown();
    Ok(())
  }

  #[test]
  fn idb_dispatch_event_swallows_exceptions_and_still_calls_listeners() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    realm.exec_script(&format!(
      "globalThis.__log = [];\n\
       globalThis.req = {{\n\
         result: 1,\n\
         onsuccess: function () {{\n\
           globalThis.__log.push('on');\n\
           throw new Error('boom');\n\
         }},\n\
       }};\n\
       globalThis.req.{LISTENERS_KEY} = {{\n\
         success: [function () {{ globalThis.__log.push('listener'); }}],\n\
       }};",
    ))?;

    let Value::Object(req_obj) = realm.exec_script("req")? else {
      panic!("expected request to be an object");
    };

    {
      let (vm, _realm, heap) = realm.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(req_obj))?;
      let microtasks_ptr = vm.microtask_queue_mut() as *mut vm_js::MicrotaskQueue;
      let mut hooks = MicrotaskQueueHooks {
        microtasks: microtasks_ptr,
      };
      let mut host = ();
      dispatch_idb_event(
        vm,
        &mut scope,
        &mut host,
        &mut hooks,
        req_obj,
        "success",
        &[],
      )?;
    }

    let json = realm.exec_script("JSON.stringify(__log)")?;
    assert_eq!(get_string(realm.heap(), json), "[\"on\",\"listener\"]");
    realm.teardown();
    Ok(())
  }

  #[test]
  fn idb_dispatch_event_applies_extra_props() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    realm.exec_script(
      "globalThis.__ok = false;\n\
       globalThis.req = {\n\
         onupgradeneeded: function (e) {\n\
           globalThis.__ok = (e.oldVersion === 1 && e.newVersion === 2);\n\
         }\n\
       };",
    )?;

    let Value::Object(req_obj) = realm.exec_script("req")? else {
      panic!("expected request to be an object");
    };

    {
      let (vm, _realm, heap) = realm.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(req_obj))?;
      let microtasks_ptr = vm.microtask_queue_mut() as *mut vm_js::MicrotaskQueue;
      let mut hooks = MicrotaskQueueHooks {
        microtasks: microtasks_ptr,
      };
      let mut host = ();
      dispatch_idb_event(
        vm,
        &mut scope,
        &mut host,
        &mut hooks,
        req_obj,
        "upgradeneeded",
        &[("oldVersion", Value::Number(1.0)), ("newVersion", Value::Number(2.0))],
      )?;
    }

    assert_eq!(realm.exec_script("__ok")?, Value::Bool(true));
    realm.teardown();
    Ok(())
  }
}
