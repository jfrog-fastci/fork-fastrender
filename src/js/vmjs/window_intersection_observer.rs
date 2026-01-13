//! Minimal `IntersectionObserver` bindings for `vm-js` Window realms.
//!
//! Many real-world scripts (lazy-loading, analytics, and various frameworks) assume
//! `IntersectionObserver` exists. FastRender does not currently implement layout/viewport
//! intersection calculations, so this module provides a **spec-shaped** implementation that
//! *eagerly* reports observed targets as intersecting.
//!
//! Semantics:
//! - `new IntersectionObserver(callback)` is supported (options are currently ignored).
//! - `observe(target)` queues a microtask (via the internal `__fastrender_queue_microtask` binding
//!   when available) that invokes the callback with entries where:
//!   - `entry.target === target`
//!   - `entry.isIntersecting === true`
//!   - `entry.intersectionRatio === 1`
//! - `takeRecords()` returns any pending queued entries (and clears them).
//! - `unobserve()`/`disconnect()` clear any pending queued entries.
//!
//! This is intentionally minimal: it focuses on API presence + callback delivery so scripts can
//! progress through lazy-load/observer gating logic during server-side execution.

use crate::js::window_dom_rect;
use crate::js::window_realm::WindowRealmUserData;
use vm_js::{
  GcObject, Heap, HostSlots, NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope,
  Value, Vm, VmError, VmHost, VmHostHooks,
};

// Brand wrapper instances as platform objects via `HostSlots` so `structuredClone` rejects them.
const INTERSECTION_OBSERVER_HOST_TAG: u64 = 0x494E_5445_524F_4253; // "INTEROBS"
const INTERSECTION_OBSERVER_ENTRY_HOST_TAG: u64 = 0x494F_4245_4E54_5259; // "IOBENTRY"

const OBSERVER_CALLBACK_KEY: &str = "__fastrender_intersection_observer_callback";
const OBSERVER_PENDING_TARGETS_KEY: &str = "__fastrender_intersection_observer_pending_targets";
const OBSERVER_SCHEDULED_KEY: &str = "__fastrender_intersection_observer_scheduled";
const OBSERVER_GLOBAL_KEY: &str = "__fastrender_intersection_observer_global";

// Must match `window_timers::INTERNAL_QUEUE_MICROTASK_KEY`, but duplicated here to keep this module
// independent of timer bindings.
const INTERNAL_QUEUE_MICROTASK_KEY: &str = "__fastrender_queue_microtask";

// Native slot indices for `IntersectionObserver.prototype.observe`.
const OBSERVE_GLOBAL_SLOT: usize = 0;
const OBSERVE_NOTIFY_CALL_ID_SLOT: usize = 1;

// Native slot indices for the `IntersectionObserver` constructor.
const CTOR_GLOBAL_SLOT: usize = 0;

// Native slot indices for the microtask notification callback.
const NOTIFY_OBSERVER_SLOT: usize = 0;

fn data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn ctor_link_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: false,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn get_own_data_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<Value, VmError> {
  // Root the object while allocating the property key: `alloc_key` can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  let key = alloc_key(&mut scope, name)?;
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)?
      .unwrap_or(Value::Undefined),
  )
}

fn set_own_data_prop(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
  writable: bool,
) -> Result<(), VmError> {
  // Root `obj` + `value` while allocating the property key: `alloc_key` can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(value, writable))
}

fn require_object(this: Value, err: &'static str) -> Result<GcObject, VmError> {
  match this {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::TypeError(err)),
  }
}

fn require_intersection_observer(scope: &Scope<'_>, this: Value, err: &'static str) -> Result<GcObject, VmError> {
  let obj = require_object(this, err)?;
  let Some(slots) = scope.heap().object_host_slots(obj)? else {
    return Err(VmError::TypeError(err));
  };
  if slots.a == INTERSECTION_OBSERVER_HOST_TAG {
    Ok(obj)
  } else {
    Err(VmError::TypeError(err))
  }
}

fn is_dom_element(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<bool, VmError> {
  // Prefer a strict platform-object check (WebIDL `Element` conversion). If the WindowRealm DOM
  // platform isn't available (e.g. in a reduced embedding), fall back to a best-effort `nodeType`
  // check so real-world scripts can still run.
  if let Some(user_data) = vm.user_data_mut::<WindowRealmUserData>() {
    if let Some(platform) = user_data.dom_platform_mut() {
      return Ok(platform.require_element_handle(scope.heap(), value).is_ok());
    }
  }

  let Value::Object(obj) = value else {
    return Ok(false);
  };
  // Root the object while allocating the property key and performing the lookup.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  let node_type_key = alloc_key(&mut scope, "nodeType")?;
  let node_type = vm.get_with_host_and_hooks(host, &mut scope, hooks, obj, node_type_key)?;
  Ok(matches!(node_type, Value::Number(n) if n == 1.0))
}

fn observe_global_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots
    .get(OBSERVE_GLOBAL_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "IntersectionObserver.observe missing required global slot",
    )),
  }
}

fn ctor_global_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.get(CTOR_GLOBAL_SLOT).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "IntersectionObserver constructor missing required global slot",
    )),
  }
}

fn notify_call_id_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<NativeFunctionId, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots
    .get(OBSERVE_NOTIFY_CALL_ID_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Number(n) if n.is_finite() && n >= 0.0 => Ok(NativeFunctionId(n as u32)),
    _ => Err(VmError::InvariantViolation(
      "IntersectionObserver.observe missing required notify call id slot",
    )),
  }
}

fn array_length(scope: &mut Scope<'_>, arr: GcObject) -> Result<usize, VmError> {
  let len = get_own_data_prop(scope, arr, "length")?;
  match len {
    Value::Number(n) if n.is_finite() && n >= 0.0 => Ok(n.trunc() as usize),
    _ => Ok(0),
  }
}

fn alloc_array_with_prototype(vm: &Vm, scope: &mut Scope<'_>, len: usize) -> Result<GcObject, VmError> {
  let arr = scope.alloc_array(len)?;
  scope.push_root(Value::Object(arr))?;
  if let Some(intrinsics) = vm.intrinsics() {
    scope
      .heap_mut()
      .object_set_prototype(arr, Some(intrinsics.array_prototype()))?;
  }
  Ok(arr)
}

fn alloc_empty_targets_array(vm: &Vm, scope: &mut Scope<'_>) -> Result<GcObject, VmError> {
  alloc_array_with_prototype(vm, scope, 0)
}

fn get_or_create_pending_targets(
  vm: &Vm,
  scope: &mut Scope<'_>,
  observer_obj: GcObject,
) -> Result<GcObject, VmError> {
  let existing = get_own_data_prop(scope, observer_obj, OBSERVER_PENDING_TARGETS_KEY)?;
  if let Value::Object(obj) = existing {
    return Ok(obj);
  }

  let arr = alloc_empty_targets_array(vm, scope)?;
  set_own_data_prop(
    scope,
    observer_obj,
    OBSERVER_PENDING_TARGETS_KEY,
    Value::Object(arr),
    /* writable */ false,
  )?;
  Ok(arr)
}

fn clear_pending_targets(vm: &Vm, scope: &mut Scope<'_>, observer_obj: GcObject) -> Result<(), VmError> {
  let empty = alloc_empty_targets_array(vm, scope)?;
  set_own_data_prop(
    scope,
    observer_obj,
    OBSERVER_PENDING_TARGETS_KEY,
    Value::Object(empty),
    /* writable */ false,
  )?;
  Ok(())
}

fn build_entries_from_pending_targets(
  vm: &Vm,
  scope: &mut Scope<'_>,
  global: Option<GcObject>,
  pending_targets: GcObject,
) -> Result<GcObject, VmError> {
  // Root the pending array while we read elements and allocate entry objects/strings.
  scope.push_root(Value::Object(pending_targets))?;
  let len = array_length(scope, pending_targets)?;

  let entries = alloc_array_with_prototype(vm, scope, len)?;
  scope.push_root(Value::Object(entries))?;

  for idx in 0..len {
    // Root intermediate values for this iteration.
    let mut iter_scope = scope.reborrow();

    let target = {
      iter_scope.push_root(Value::Object(pending_targets))?;
      let key = alloc_key(&mut iter_scope, &idx.to_string())?;
      iter_scope
        .heap()
        .object_get_own_data_property_value(pending_targets, &key)?
        .unwrap_or(Value::Undefined)
    };

    let entry = iter_scope.alloc_object()?;
    iter_scope.push_root(Value::Object(entry))?;
    iter_scope.heap_mut().object_set_host_slots(
      entry,
      HostSlots {
        a: INTERSECTION_OBSERVER_ENTRY_HOST_TAG,
        b: 0,
      },
    )?;
    iter_scope.push_root(target)?;

    set_own_data_prop(
      &mut iter_scope,
      entry,
      "target",
      target,
      /* writable */ false,
    )?;
    set_own_data_prop(
      &mut iter_scope,
      entry,
      "isIntersecting",
      Value::Bool(true),
      /* writable */ false,
    )?;
    set_own_data_prop(
      &mut iter_scope,
      entry,
      "intersectionRatio",
      Value::Number(1.0),
      /* writable */ false,
    )?;

    // Spec-ish geometry shape: provide `DOMRectReadOnly` instances for rect fields.
    if let Some(global) = global {
      // The shim does not have real layout info; use a stable non-zero rect so `intersectionRatio == 1`
      // remains plausible (area(intersectionRect) / area(boundingClientRect) == 1).
      let root_bounds =
        window_dom_rect::alloc_dom_rect_read_only_from_global(&mut iter_scope, global, 0.0, 0.0, 1.0, 1.0)?;
      set_own_data_prop(
        &mut iter_scope,
        entry,
        "rootBounds",
        Value::Object(root_bounds),
        /* writable */ false,
      )?;

      let bounding_rect =
        window_dom_rect::alloc_dom_rect_read_only_from_global(&mut iter_scope, global, 0.0, 0.0, 1.0, 1.0)?;
      set_own_data_prop(
        &mut iter_scope,
        entry,
        "boundingClientRect",
        Value::Object(bounding_rect),
        /* writable */ false,
      )?;

      let intersection_rect =
        window_dom_rect::alloc_dom_rect_read_only_from_global(&mut iter_scope, global, 0.0, 0.0, 1.0, 1.0)?;
      set_own_data_prop(
        &mut iter_scope,
        entry,
        "intersectionRect",
        Value::Object(intersection_rect),
        /* writable */ false,
      )?;
    }

    // entries[idx] = entry
    iter_scope.push_root(Value::Object(entries))?;
    let idx_key = alloc_key(&mut iter_scope, &idx.to_string())?;
    iter_scope.define_property(
      entries,
      idx_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Object(entry),
          writable: true,
        },
      },
    )?;
  }

  Ok(entries)
}

fn find_queue_microtask(scope: &mut Scope<'_>, global: GcObject) -> Result<Value, VmError> {
  // Find the internal queueMicrotask implementation (preferred) or fall back to the user-visible
  // `queueMicrotask` binding.
  scope.push_root(Value::Object(global))?;
  let key = alloc_key(scope, INTERNAL_QUEUE_MICROTASK_KEY)?;
  let internal = scope
    .heap()
    .object_get_own_data_property_value(global, &key)?
    .unwrap_or(Value::Undefined);
  if matches!(internal, Value::Object(_)) && scope.heap().is_callable(internal).unwrap_or(false) {
    return Ok(internal);
  }

  let key = alloc_key(scope, "queueMicrotask")?;
  let user = scope
    .heap()
    .object_get_own_data_property_value(global, &key)?
    .unwrap_or(Value::Undefined);
  Ok(user)
}

fn deliver_pending_entries(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  observer_obj: GcObject,
) -> Result<(), VmError> {
  // Root observer while we allocate keys and objects.
  scope.push_root(Value::Object(observer_obj))?;

  let callback = get_own_data_prop(scope, observer_obj, OBSERVER_CALLBACK_KEY)?;
  if !matches!(callback, Value::Object(_)) || !scope.heap().is_callable(callback)? {
    set_own_data_prop(
      scope,
      observer_obj,
      OBSERVER_SCHEDULED_KEY,
      Value::Bool(false),
      /* writable */ true,
    )?;
    clear_pending_targets(vm, scope, observer_obj)?;
    return Ok(());
  }

  let pending_targets = match get_own_data_prop(scope, observer_obj, OBSERVER_PENDING_TARGETS_KEY)? {
    Value::Object(obj) => obj,
    _ => {
      set_own_data_prop(
        scope,
        observer_obj,
        OBSERVER_SCHEDULED_KEY,
        Value::Bool(false),
        /* writable */ true,
      )?;
      return Ok(());
    }
  };

  let pending_len = {
    let mut len_scope = scope.reborrow();
    len_scope.push_root(Value::Object(pending_targets))?;
    array_length(&mut len_scope, pending_targets)?
  };

  if pending_len == 0 {
    set_own_data_prop(
      scope,
      observer_obj,
      OBSERVER_SCHEDULED_KEY,
      Value::Bool(false),
      /* writable */ true,
    )?;
    return Ok(());
  }

  let global = match get_own_data_prop(scope, observer_obj, OBSERVER_GLOBAL_KEY)? {
    Value::Object(obj) => Some(obj),
    _ => None,
  };

  let entries = build_entries_from_pending_targets(vm, scope, global, pending_targets)?;
  scope.push_root(Value::Object(entries))?;

  // Clear pending state before invoking the callback so re-entrancy behaves sensibly.
  clear_pending_targets(vm, scope, observer_obj)?;
  set_own_data_prop(
    scope,
    observer_obj,
    OBSERVER_SCHEDULED_KEY,
    Value::Bool(false),
    /* writable */ true,
  )?;

  let args = [Value::Object(entries), Value::Object(observer_obj)];
  // Per web platform behavior, exceptions from IntersectionObserver callbacks should not abort the
  // microtask checkpoint.
  let _ = vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    callback,
    Value::Object(observer_obj),
    &args,
  );
  Ok(())
}

fn intersection_observer_notify_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let observer_obj = match slots
    .get(NOTIFY_OBSERVER_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => return Ok(Value::Undefined),
  };

  // Best-effort delivery; never throw from the queued microtask.
  let _ = deliver_pending_entries(vm, scope, host, hooks, observer_obj);
  Ok(Value::Undefined)
}

fn intersection_observer_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "IntersectionObserver constructor cannot be invoked without 'new'",
  ))
}

fn intersection_observer_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let global = ctor_global_from_callee(scope, callee)?;

  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if !scope.heap().is_callable(callback).unwrap_or(false) {
    return Err(VmError::TypeError(
      "IntersectionObserver callback is not callable",
    ));
  }

  let ctor = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };

  let prototype_key = alloc_key(scope, "prototype")?;
  let proto = scope
    .heap()
    .object_get_own_data_property_value(ctor, &prototype_key)?
    .and_then(|v| match v {
      Value::Object(obj) => Some(obj),
      _ => None,
    });

  let observer = scope.alloc_object()?;
  scope.push_root(Value::Object(observer))?;
  if let Some(proto) = proto {
    scope.heap_mut().object_set_prototype(observer, Some(proto))?;
  }

  // Internal state.
  scope.heap_mut().object_set_host_slots(
    observer,
    HostSlots {
      a: INTERSECTION_OBSERVER_HOST_TAG,
      b: 0,
    },
  )?;
  set_own_data_prop(
    scope,
    observer,
    OBSERVER_CALLBACK_KEY,
    callback,
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    observer,
    OBSERVER_SCHEDULED_KEY,
    Value::Bool(false),
    /* writable */ true,
  )?;
  set_own_data_prop(
    scope,
    observer,
    OBSERVER_GLOBAL_KEY,
    Value::Object(global),
    /* writable */ false,
  )?;
  let pending = alloc_empty_targets_array(vm, scope)?;
  set_own_data_prop(
    scope,
    observer,
    OBSERVER_PENDING_TARGETS_KEY,
    Value::Object(pending),
    /* writable */ false,
  )?;

  // Public-ish properties commonly inspected by libraries.
  set_own_data_prop(scope, observer, "root", Value::Null, /* writable */ false)?;
  let root_margin = scope.alloc_string("0px 0px 0px 0px")?;
  scope.push_root(Value::String(root_margin))?;
  set_own_data_prop(
    scope,
    observer,
    "rootMargin",
    Value::String(root_margin),
    /* writable */ false,
  )?;
  let thresholds = alloc_array_with_prototype(vm, scope, 1)?;
  scope.push_root(Value::Object(thresholds))?;
  let idx_key = alloc_key(scope, "0")?;
  scope.define_property(
    thresholds,
    idx_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Number(0.0),
        writable: true,
      },
    },
  )?;
  set_own_data_prop(
    scope,
    observer,
    "thresholds",
    Value::Object(thresholds),
    /* writable */ false,
  )?;

  Ok(Value::Object(observer))
}

fn intersection_observer_observe_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let observer_obj =
    require_intersection_observer(scope, this, "IntersectionObserver.observe: illegal invocation")?;

  let target = args.get(0).copied().unwrap_or(Value::Undefined);
  if !is_dom_element(vm, scope, host, hooks, target)? {
    return Err(VmError::TypeError(
      "IntersectionObserver.observe expects an Element",
    ));
  }

  let pending_targets = get_or_create_pending_targets(vm, scope, observer_obj)?;
  {
    // Root while we read length and define the new array index property.
    let mut append_scope = scope.reborrow();
    append_scope.push_root(Value::Object(pending_targets))?;
    append_scope.push_root(target)?;

    let idx = array_length(&mut append_scope, pending_targets)?;
    let idx_key = alloc_key(&mut append_scope, &idx.to_string())?;
    append_scope.define_property(
      pending_targets,
      idx_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: target,
          writable: true,
        },
      },
    )?;
  }

  let already_scheduled = matches!(
    get_own_data_prop(scope, observer_obj, OBSERVER_SCHEDULED_KEY)?,
    Value::Bool(true)
  );
  if already_scheduled {
    return Ok(Value::Undefined);
  }

  set_own_data_prop(
    scope,
    observer_obj,
    OBSERVER_SCHEDULED_KEY,
    Value::Bool(true),
    /* writable */ true,
  )?;

  let global = observe_global_from_callee(scope, callee)?;
  let queue_microtask = find_queue_microtask(scope, global)?;
  if matches!(queue_microtask, Value::Object(_)) && scope.heap().is_callable(queue_microtask)? {
    let notify_call_id = notify_call_id_from_callee(scope, callee)?;
    let name = scope.alloc_string("IntersectionObserver microtask")?;
    scope.push_root(Value::String(name))?;
    let notify = scope.alloc_native_function_with_slots(
      notify_call_id,
      None,
      name,
      0,
      &[Value::Object(observer_obj)],
    )?;
    scope.heap_mut().object_set_prototype(
      notify,
      Some(vm.intrinsics().ok_or(VmError::Unimplemented("missing intrinsics"))?.function_prototype()),
    )?;
    scope.push_root(Value::Object(notify))?;

    let scheduled = vm.call_with_host_and_hooks(
      host,
      scope,
      hooks,
      queue_microtask,
      Value::Undefined,
      &[Value::Object(notify)],
    );
    if scheduled.is_err() {
      // Best-effort fallback: deliver synchronously if we couldn't schedule.
      let _ = deliver_pending_entries(vm, scope, host, hooks, observer_obj);
    }
  } else {
    // No queueMicrotask: deliver synchronously.
    let _ = deliver_pending_entries(vm, scope, host, hooks, observer_obj);
  }

  Ok(Value::Undefined)
}

fn intersection_observer_disconnect_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let observer_obj =
    require_intersection_observer(scope, this, "IntersectionObserver.disconnect: illegal invocation")?;
  clear_pending_targets(vm, scope, observer_obj)?;
  set_own_data_prop(
    scope,
    observer_obj,
    OBSERVER_SCHEDULED_KEY,
    Value::Bool(false),
    /* writable */ true,
  )?;
  Ok(Value::Undefined)
}

fn intersection_observer_take_records_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let observer_obj =
    require_intersection_observer(scope, this, "IntersectionObserver.takeRecords: illegal invocation")?;

  let pending_targets = match get_own_data_prop(scope, observer_obj, OBSERVER_PENDING_TARGETS_KEY)? {
    Value::Object(obj) => obj,
    _ => alloc_empty_targets_array(vm, scope)?,
  };

  let global = match get_own_data_prop(scope, observer_obj, OBSERVER_GLOBAL_KEY)? {
    Value::Object(obj) => Some(obj),
    _ => None,
  };

  let entries = build_entries_from_pending_targets(vm, scope, global, pending_targets)?;
  clear_pending_targets(vm, scope, observer_obj)?;
  set_own_data_prop(
    scope,
    observer_obj,
    OBSERVER_SCHEDULED_KEY,
    Value::Bool(false),
    /* writable */ true,
  )?;
  Ok(Value::Object(entries))
}

fn intersection_observer_unobserve_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let observer_obj =
    require_intersection_observer(scope, this, "IntersectionObserver.unobserve: illegal invocation")?;

  let target = args.get(0).copied().unwrap_or(Value::Undefined);
  if !is_dom_element(vm, scope, host, hooks, target)? {
    return Err(VmError::TypeError(
      "IntersectionObserver.unobserve expects an Element",
    ));
  }

  let pending_targets = match get_own_data_prop(scope, observer_obj, OBSERVER_PENDING_TARGETS_KEY)? {
    Value::Object(obj) => obj,
    _ => return Ok(Value::Undefined),
  };

  // Remove `target` from the pending queue (best-effort, since we don't maintain a full observed set).
  scope.push_root(Value::Object(pending_targets))?;
  scope.push_root(target)?;

  let len = array_length(scope, pending_targets)?;
  if len == 0 {
    return Ok(Value::Undefined);
  }

  let mut kept = 0usize;
  for idx in 0..len {
    let mut iter_scope = scope.reborrow();
    iter_scope.push_root(Value::Object(pending_targets))?;
    let key = alloc_key(&mut iter_scope, &idx.to_string())?;
    let value = iter_scope
      .heap()
      .object_get_own_data_property_value(pending_targets, &key)?
      .unwrap_or(Value::Undefined);
    if value != target {
      kept += 1;
    }
  }

  if kept == len {
    return Ok(Value::Undefined);
  }

  let new_pending = alloc_array_with_prototype(vm, scope, kept)?;
  scope.push_root(Value::Object(new_pending))?;

  let mut out_idx = 0usize;
  for idx in 0..len {
    let mut iter_scope = scope.reborrow();
    iter_scope.push_root(Value::Object(pending_targets))?;
    let key = alloc_key(&mut iter_scope, &idx.to_string())?;
    let value = iter_scope
      .heap()
      .object_get_own_data_property_value(pending_targets, &key)?
      .unwrap_or(Value::Undefined);
    if value == target {
      continue;
    }

    iter_scope.push_root(Value::Object(new_pending))?;
    let out_key = alloc_key(&mut iter_scope, &out_idx.to_string())?;
    iter_scope.define_property(new_pending, out_key, data_desc(value, true))?;
    out_idx += 1;
  }

  set_own_data_prop(
    scope,
    observer_obj,
    OBSERVER_PENDING_TARGETS_KEY,
    Value::Object(new_pending),
    /* writable */ false,
  )?;

  Ok(Value::Undefined)
}

/// Install `IntersectionObserver` onto the global object of a `vm-js` Window realm.
pub fn install_window_intersection_observer_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
) -> Result<(), VmError> {
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let func_proto = realm.intrinsics().function_prototype();

  let notify_call_id = vm.register_native_call(intersection_observer_notify_native)?;

  // --- IntersectionObserver.prototype ---------------------------------------------------------
  let proto = scope.alloc_object()?;
  scope.push_root(Value::Object(proto))?;
  scope
    .heap_mut()
    .object_set_prototype(proto, Some(realm.intrinsics().object_prototype()))?;

  let observe_call_id = vm.register_native_call(intersection_observer_observe_native)?;
  let observe_name = scope.alloc_string("observe")?;
  scope.push_root(Value::String(observe_name))?;
  let observe_slots = [
    Value::Object(global),
    Value::Number(notify_call_id.0 as f64),
  ];
  let observe = scope.alloc_native_function_with_slots(
    observe_call_id,
    None,
    observe_name,
    1,
    &observe_slots,
  )?;
  scope.heap_mut().object_set_prototype(observe, Some(func_proto))?;
  set_own_data_prop(&mut scope, proto, "observe", Value::Object(observe), /* writable */ true)?;

  let unobserve_call_id = vm.register_native_call(intersection_observer_unobserve_native)?;
  let unobserve_name = scope.alloc_string("unobserve")?;
  scope.push_root(Value::String(unobserve_name))?;
  let unobserve = scope.alloc_native_function(unobserve_call_id, None, unobserve_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(unobserve, Some(func_proto))?;
  set_own_data_prop(
    &mut scope,
    proto,
    "unobserve",
    Value::Object(unobserve),
    /* writable */ true,
  )?;

  let disconnect_call_id = vm.register_native_call(intersection_observer_disconnect_native)?;
  let disconnect_name = scope.alloc_string("disconnect")?;
  scope.push_root(Value::String(disconnect_name))?;
  let disconnect = scope.alloc_native_function(disconnect_call_id, None, disconnect_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(disconnect, Some(func_proto))?;
  set_own_data_prop(
    &mut scope,
    proto,
    "disconnect",
    Value::Object(disconnect),
    /* writable */ true,
  )?;

  let take_records_call_id = vm.register_native_call(intersection_observer_take_records_native)?;
  let take_records_name = scope.alloc_string("takeRecords")?;
  scope.push_root(Value::String(take_records_name))?;
  let take_records = scope.alloc_native_function(take_records_call_id, None, take_records_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(take_records, Some(func_proto))?;
  set_own_data_prop(
    &mut scope,
    proto,
    "takeRecords",
    Value::Object(take_records),
    /* writable */ true,
  )?;

  // --- IntersectionObserver (constructible) ---------------------------------------------------
  let ctor_call_id = vm.register_native_call(intersection_observer_ctor_call)?;
  let ctor_construct_id = vm.register_native_construct(intersection_observer_ctor_construct)?;
  let name = scope.alloc_string("IntersectionObserver")?;
  scope.push_root(Value::String(name))?;
  let ctor = scope.alloc_native_function_with_slots(
    ctor_call_id,
    Some(ctor_construct_id),
    name,
    1,
    &[Value::Object(global)],
  )?;
  scope.heap_mut().object_set_prototype(ctor, Some(func_proto))?;
  scope.push_root(Value::Object(ctor))?;

  // Link constructor <-> prototype.
  let prototype_key = alloc_key(&mut scope, "prototype")?;
  let constructor_key = alloc_key(&mut scope, "constructor")?;
  scope.define_property(ctor, prototype_key, ctor_link_desc(Value::Object(proto)))?;
  scope.define_property(proto, constructor_key, ctor_link_desc(Value::Object(ctor)))?;

  // Expose on global.
  let key = alloc_key(&mut scope, "IntersectionObserver")?;
  scope.define_property(global, key, data_desc(Value::Object(ctor), true))?;

  Ok(())
}
