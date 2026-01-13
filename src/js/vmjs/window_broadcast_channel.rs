//! Minimal `BroadcastChannel` implementation for `vm-js` Window realms.
//!
//! This is an MVP that provides same-origin, in-process broadcast for real-world libraries that use
//! `BroadcastChannel` for cross-context coordination.
//!
//! - Channels are keyed by `(origin, name)`.
//! - Messages are cloned via a small, host-side structured-clone subset (enough for common usage).
//! - Delivery is synchronous (runs the receiver's JS handlers immediately) and best-effort:
//!   exceptions thrown by handlers are swallowed.

use crate::js::window_realm::WindowRealmUserData;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use url::Url;
use vm_js::{
  GcObject, GcString, Heap, HostSlots, PropertyDescriptor, PropertyKey, PropertyKind, Realm, RealmId, Scope,
  Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};

// Brand wrapper instances as platform objects via `HostSlots` so `structuredClone` rejects them.
const BROADCAST_CHANNEL_HOST_TAG: u64 = 0x4252_4F41_4443_484E; // "BROADCHN"

const INTERNAL_NAME_KEY: &str = "__fastrender_broadcast_channel_name";
const INTERNAL_ORIGIN_KEY: &str = "__fastrender_broadcast_channel_origin";
const INTERNAL_CLOSED_KEY: &str = "__fastrender_broadcast_channel_closed";
const INTERNAL_LISTENERS_KEY: &str = "__fastrender_broadcast_channel_listeners";

const ILLEGAL_INVOCATION_ERROR: &str = "BroadcastChannel: illegal invocation";
const CONSTRUCTOR_REQUIRES_NEW_ERROR: &str = "BroadcastChannel constructor cannot be invoked without 'new'";
const CLOSED_ERROR: &str = "BroadcastChannel is closed";

/// Hard cap on the UTF-16 length of the channel name.
const MAX_CHANNEL_NAME_UNITS: usize = 1024;

/// Hard cap on serialized message bytes (rough approximation).
const MAX_MESSAGE_BYTES: usize = 10 * 1024 * 1024;

/// Hard cap on number of nodes visited during message serialization (DoS resistance).
const MAX_MESSAGE_NODES: usize = 100_000;

/// Hard cap on number of live channels per `(origin, name)` tuple.
const MAX_CHANNELS_PER_KEY: usize = 1024;

/// Hard cap on listeners registered on a single BroadcastChannel instance.
const MAX_LISTENERS_PER_CHANNEL: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RegistryKey {
  origin: String,
  name: String,
}

#[derive(Clone, Copy)]
struct ChannelEntry {
  obj: WeakGcObject,
  vm_ptr: usize,
  heap_ptr: usize,
  realm_id: RealmId,
  last_gc_runs: u64,
}

#[derive(Default)]
struct BroadcastChannelRegistry {
  channels: HashMap<RegistryKey, Vec<ChannelEntry>>,
}

static REGISTRY: OnceLock<Mutex<BroadcastChannelRegistry>> = OnceLock::new();

fn registry() -> &'static Mutex<BroadcastChannelRegistry> {
  REGISTRY.get_or_init(|| Mutex::new(BroadcastChannelRegistry::default()))
}

fn data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn internal_data_desc(value: Value) -> PropertyDescriptor {
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

fn serialized_origin_for_current_realm(vm: &Vm) -> Result<String, VmError> {
  let Some(data) = vm.user_data::<WindowRealmUserData>() else {
    return Err(VmError::InvariantViolation(
      "BroadcastChannel requires WindowRealmUserData",
    ));
  };
  let Ok(url) = Url::parse(data.document_url()) else {
    return Ok("null".to_string());
  };
  match url.scheme() {
    "http" | "https" => Ok(url.origin().ascii_serialization()),
    _ => Ok("null".to_string()),
  }
}

fn require_channel(scope: &Scope<'_>, this: Value) -> Result<GcObject, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError(ILLEGAL_INVOCATION_ERROR));
  };
  let Some(slots) = scope.heap().object_host_slots(obj)? else {
    return Err(VmError::TypeError(ILLEGAL_INVOCATION_ERROR));
  };
  if slots.a == BROADCAST_CHANNEL_HOST_TAG {
    Ok(obj)
  } else {
    Err(VmError::TypeError(ILLEGAL_INVOCATION_ERROR))
  }
}

fn channel_is_closed(scope: &mut Scope<'_>, obj: GcObject) -> Result<bool, VmError> {
  scope.push_root(Value::Object(obj))?;
  let key = alloc_key(scope, INTERNAL_CLOSED_KEY)?;
  Ok(matches!(
    scope.heap().object_get_own_data_property_value(obj, &key)?,
    Some(Value::Bool(true))
  ))
}

fn channel_registry_key(scope: &mut Scope<'_>, obj: GcObject) -> Result<RegistryKey, VmError> {
  scope.push_root(Value::Object(obj))?;

  let name_key = alloc_key(scope, INTERNAL_NAME_KEY)?;
  let origin_key = alloc_key(scope, INTERNAL_ORIGIN_KEY)?;

  let name_val = scope
    .heap()
    .object_get_own_data_property_value(obj, &name_key)?
    .unwrap_or(Value::Undefined);
  let origin_val = scope
    .heap()
    .object_get_own_data_property_value(obj, &origin_key)?
    .unwrap_or(Value::Undefined);

  let Value::String(name_s) = name_val else {
    return Err(VmError::InvariantViolation(
      "BroadcastChannel missing internal name",
    ));
  };
  let Value::String(origin_s) = origin_val else {
    return Err(VmError::InvariantViolation(
      "BroadcastChannel missing internal origin",
    ));
  };

  let name = scope
    .heap()
    .get_string(name_s)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();
  let origin = scope
    .heap()
    .get_string(origin_s)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  Ok(RegistryKey { origin, name })
}

// --- Structured clone (subset) ---------------------------------------------------------------

#[derive(Debug)]
enum SerializedValue {
  Undefined,
  Null,
  Bool(bool),
  Number(f64),
  BigInt(vm_js::JsBigInt),
  String(Vec<u16>),
  Array(Vec<SerializedValue>),
  Object(Vec<(Vec<u16>, SerializedValue)>),
  ArrayBuffer(Vec<u8>),
  Uint8Array(Vec<u8>),
}

#[derive(Clone, Copy, Debug)]
struct SerializeBudget {
  bytes: usize,
  nodes: usize,
}

fn budget_add_bytes(budget: &mut SerializeBudget, add: usize) -> Result<(), VmError> {
  budget.bytes = budget.bytes.checked_add(add).ok_or(VmError::OutOfMemory)?;
  if budget.bytes > MAX_MESSAGE_BYTES {
    return Err(VmError::TypeError("BroadcastChannel message exceeds size limits"));
  }
  Ok(())
}

fn budget_add_node(budget: &mut SerializeBudget) -> Result<(), VmError> {
  budget.nodes = budget.nodes.checked_add(1).ok_or(VmError::OutOfMemory)?;
  if budget.nodes > MAX_MESSAGE_NODES {
    return Err(VmError::TypeError("BroadcastChannel message exceeds size limits"));
  }
  Ok(())
}

fn string_to_code_units(heap: &Heap, s: GcString, budget: &mut SerializeBudget) -> Result<Vec<u16>, VmError> {
  let units = heap.get_string(s)?.as_code_units();
  budget_add_bytes(budget, units.len().saturating_mul(2))?;
  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve_exact(units.len())
    .map_err(|_| VmError::OutOfMemory)?;
  out.extend_from_slice(units);
  Ok(out)
}

fn serialize_value(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  value: Value,
  depth: usize,
  budget: &mut SerializeBudget,
) -> Result<SerializedValue, VmError> {
  if depth > 256 {
    return Err(VmError::TypeError(
      "BroadcastChannel message exceeds maximum depth",
    ));
  }
  budget_add_node(budget)?;

  match value {
    Value::Undefined => Ok(SerializedValue::Undefined),
    Value::Null => Ok(SerializedValue::Null),
    Value::Bool(b) => Ok(SerializedValue::Bool(b)),
    Value::Number(n) => Ok(SerializedValue::Number(n)),
    Value::BigInt(b) => {
      let bi = scope.heap().get_bigint(b)?;
      budget_add_bytes(budget, bi.estimated_byte_len())?;
      Ok(SerializedValue::BigInt(bi.try_clone()?))
    }
    Value::String(s) => Ok(SerializedValue::String(string_to_code_units(
      scope.heap(),
      s,
      budget,
    )?)),
    Value::Symbol(_) => Err(VmError::TypeError(
      "BroadcastChannel message contains a non-cloneable value",
    )),
    Value::Object(obj) => {
      // Reject callables (functions).
      if scope.heap().is_callable(Value::Object(obj))? {
        return Err(VmError::TypeError(
          "BroadcastChannel message contains a non-cloneable value",
        ));
      }

      if scope.heap().is_array_buffer_object(obj) {
        let data = scope.heap().array_buffer_data(obj)?;
        budget_add_bytes(budget, data.len())?;
        let mut out: Vec<u8> = Vec::new();
        out
          .try_reserve_exact(data.len())
          .map_err(|_| VmError::OutOfMemory)?;
        out.extend_from_slice(data);
        return Ok(SerializedValue::ArrayBuffer(out));
      }

      if scope.heap().is_uint8_array_object(obj) {
        let data = scope.heap().uint8_array_data(obj)?;
        budget_add_bytes(budget, data.len())?;
        let mut out: Vec<u8> = Vec::new();
        out
          .try_reserve_exact(data.len())
          .map_err(|_| VmError::OutOfMemory)?;
        out.extend_from_slice(data);
        return Ok(SerializedValue::Uint8Array(out));
      }

      if scope.heap().object_is_array(obj)? {
        let len = {
          let len_key = alloc_key(scope, "length")?;
          match scope.heap().object_get_own_data_property_value(obj, &len_key)? {
            Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
            _ => 0,
          }
        };
        let mut out: Vec<SerializedValue> = Vec::new();
        out
          .try_reserve_exact(len.min(32))
          .map_err(|_| VmError::OutOfMemory)?;
        for i in 0..len {
          // Keep budgets cooperative when cloning large arrays: `ordinary_own_property_keys` can tick
          // internally, but the per-index loop may not otherwise tick.
          if i % 1024 == 0 {
            vm.tick()?;
          }
          let key = alloc_key(scope, &i.to_string())?;
          let v = scope
            .heap()
            .object_get_own_data_property_value(obj, &key)?
            .unwrap_or(Value::Undefined);
          out.push(serialize_value(vm, scope, v, depth + 1, budget)?);
        }
        return Ok(SerializedValue::Array(out));
      }

      // Ordinary objects: clone own string-keyed data properties.
      let keys = scope.ordinary_own_property_keys_with_tick(obj, || vm.tick())?;
      let mut props: Vec<(Vec<u16>, SerializedValue)> = Vec::new();
      for key in keys {
        let PropertyKey::String(key_s) = key else {
          continue;
        };
        let units = string_to_code_units(scope.heap(), key_s, budget)?;
        let v = scope
          .heap()
          .object_get_own_data_property_value(obj, &PropertyKey::from_string(key_s))?
          .unwrap_or(Value::Undefined);
        props.push((units, serialize_value(vm, scope, v, depth + 1, budget)?));
      }
      Ok(SerializedValue::Object(props))
    }
  }
}

fn deserialize_value(vm: &mut Vm, scope: &mut Scope<'_>, value: &SerializedValue) -> Result<Value, VmError> {
  match value {
    SerializedValue::Undefined => Ok(Value::Undefined),
    SerializedValue::Null => Ok(Value::Null),
    SerializedValue::Bool(b) => Ok(Value::Bool(*b)),
    SerializedValue::Number(n) => Ok(Value::Number(*n)),
    SerializedValue::BigInt(b) => Ok(Value::BigInt(scope.alloc_bigint(b.try_clone()?)?)),
    SerializedValue::String(units) => {
      let s = scope.alloc_string_from_code_units(units)?;
      scope.push_root(Value::String(s))?;
      Ok(Value::String(s))
    }
    SerializedValue::Array(items) => {
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("BroadcastChannel requires intrinsics"))?;
      let arr = scope.alloc_array(items.len())?;
      scope.push_root(Value::Object(arr))?;
      scope
        .heap_mut()
        .object_set_prototype(arr, Some(intr.array_prototype()))?;
      for (i, item) in items.iter().enumerate() {
        let v = deserialize_value(vm, scope, item)?;
        scope.push_root(v)?;
        let key = alloc_key(scope, &i.to_string())?;
        scope.define_property(
          arr,
          key,
          PropertyDescriptor {
            enumerable: true,
            configurable: true,
            kind: PropertyKind::Data { value: v, writable: true },
          },
        )?;
      }
      Ok(Value::Object(arr))
    }
    SerializedValue::Object(props) => {
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("BroadcastChannel requires intrinsics"))?;
      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;
      scope
        .heap_mut()
        .object_set_prototype(obj, Some(intr.object_prototype()))?;
      for (key_units, item) in props {
        let v = deserialize_value(vm, scope, item)?;
        // Root object + value while allocating the property key string.
        let mut inner = scope.reborrow();
        inner.push_root(Value::Object(obj))?;
        inner.push_root(v)?;
        let key_s = inner.alloc_string_from_code_units(key_units)?;
        inner.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);
        inner.define_property(obj, key, data_desc(v, true))?;
      }
      Ok(Value::Object(obj))
    }
    SerializedValue::ArrayBuffer(bytes) => {
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("BroadcastChannel requires intrinsics"))?;
      let ab = scope.alloc_array_buffer_from_u8_vec(bytes.clone())?;
      scope.push_root(Value::Object(ab))?;
      scope
        .heap_mut()
        .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
      Ok(Value::Object(ab))
    }
    SerializedValue::Uint8Array(bytes) => {
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("BroadcastChannel requires intrinsics"))?;
      let ab = scope.alloc_array_buffer_from_u8_vec(bytes.clone())?;
      scope.push_root(Value::Object(ab))?;
      scope
        .heap_mut()
        .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
      let view = scope.alloc_uint8_array(ab, 0, bytes.len())?;
      scope.push_root(Value::Object(view))?;
      scope
        .heap_mut()
        .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
      Ok(Value::Object(view))
    }
  }
}

// --- Event delivery -------------------------------------------------------------------------

fn get_or_create_listeners_array(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  channel_obj: GcObject,
) -> Result<GcObject, VmError> {
  let key = alloc_key(scope, INTERNAL_LISTENERS_KEY)?;
  if let Some(Value::Object(arr)) = scope.heap().object_get_own_data_property_value(channel_obj, &key)? {
    return Ok(arr);
  }

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("BroadcastChannel requires intrinsics"))?;
  let arr = scope.alloc_array(0)?;
  scope.push_root(Value::Object(arr))?;
  scope
    .heap_mut()
    .object_set_prototype(arr, Some(intr.array_prototype()))?;
  scope.define_property(channel_obj, key, internal_data_desc(Value::Object(arr)))?;
  Ok(arr)
}

fn listeners_array_len(scope: &mut Scope<'_>, arr: GcObject) -> Result<usize, VmError> {
  scope.push_root(Value::Object(arr))?;
  let len_key = alloc_key(scope, "length")?;
  match scope.heap().object_get_own_data_property_value(arr, &len_key)? {
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
  if !matches!(callee, Value::Object(_)) {
    return;
  }
  if scope.heap().is_callable(callee).unwrap_or(false) {
    let _ = vm.call_with_host_and_hooks(host, scope, hooks, callee, this, args);
  }
}

fn dispatch_message_event(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  channel_obj: GcObject,
  message: Value,
  origin: &str,
) -> Result<(), VmError> {
  // Create the event object: { type: "message", data, origin }.
  let event_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(event_obj))?;

  let type_s = scope.alloc_string("message")?;
  scope.push_root(Value::String(type_s))?;
  let type_key = alloc_key(scope, "type")?;
  scope.define_property(event_obj, type_key, data_desc(Value::String(type_s), false))?;

  scope.push_root(message)?;
  let data_key = alloc_key(scope, "data")?;
  scope.define_property(event_obj, data_key, data_desc(message, false))?;

  let origin_s = scope.alloc_string(origin)?;
  scope.push_root(Value::String(origin_s))?;
  let origin_key = alloc_key(scope, "origin")?;
  scope.define_property(event_obj, origin_key, data_desc(Value::String(origin_s), false))?;

  // Call onmessage first (matches common browser behavior: attribute handler participates).
  scope.push_root(Value::Object(channel_obj))?;
  let onmessage_key = alloc_key(scope, "onmessage")?;
  let onmessage = scope
    .heap()
    .object_get_own_data_property_value(channel_obj, &onmessage_key)?
    .unwrap_or(Value::Undefined);
  call_if_callable(
    vm,
    scope,
    host,
    hooks,
    onmessage,
    Value::Object(channel_obj),
    &[Value::Object(event_obj)],
  );

  // Call listeners added via addEventListener.
  let listeners_key = alloc_key(scope, INTERNAL_LISTENERS_KEY)?;
  let Some(Value::Object(listeners_arr)) =
    scope.heap().object_get_own_data_property_value(channel_obj, &listeners_key)?
  else {
    return Ok(());
  };

  let len = listeners_array_len(scope, listeners_arr)?;
  for i in 0..len {
    let key = alloc_key(scope, &i.to_string())?;
    let listener = scope
      .heap()
      .object_get_own_data_property_value(listeners_arr, &key)?
      .unwrap_or(Value::Undefined);
    if matches!(listener, Value::Undefined | Value::Null) {
      continue;
    }
    call_if_callable(
      vm,
      scope,
      host,
      hooks,
      listener,
      Value::Object(channel_obj),
      &[Value::Object(event_obj)],
    );
  }

  Ok(())
}

// --- Native bindings ------------------------------------------------------------------------

fn broadcast_channel_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(CONSTRUCTOR_REQUIRES_NEW_ERROR))
}

fn broadcast_channel_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "BroadcastChannel requires intrinsics (create a Realm first)",
  ))?;

  // Parse name before touching global registry: ToString can invoke user code.
  let name_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let name_s = scope.to_string(vm, host, hooks, name_arg)?;
  let name_units = scope.heap().get_string(name_s)?.as_code_units();
  if name_units.len() > MAX_CHANNEL_NAME_UNITS {
    return Err(VmError::TypeError("BroadcastChannel name is too long"));
  }
  let name = scope
    .heap()
    .get_string(name_s)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let origin = serialized_origin_for_current_realm(vm)?;

  // Determine prototype from newTarget.prototype (or fallback to callee.prototype).
  let ctor_obj = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };
  scope.push_root(Value::Object(ctor_obj))?;
  let proto = {
    let prototype_key = alloc_key(scope, "prototype")?;
    scope
      .heap()
      .object_get_own_data_property_value(ctor_obj, &prototype_key)?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      })
      .unwrap_or(intr.object_prototype())
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  // Brand + internal state.
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: BROADCAST_CHANNEL_HOST_TAG,
      b: 0,
    },
  )?;

  let name_key = alloc_key(scope, INTERNAL_NAME_KEY)?;
  scope.push_root(Value::String(name_s))?;
  scope.define_property(obj, name_key, internal_data_desc(Value::String(name_s)))?;

  let origin_key = alloc_key(scope, INTERNAL_ORIGIN_KEY)?;
  let origin_s = scope.alloc_string(&origin)?;
  scope.push_root(Value::String(origin_s))?;
  scope.define_property(obj, origin_key, internal_data_desc(Value::String(origin_s)))?;

  let closed_key = alloc_key(scope, INTERNAL_CLOSED_KEY)?;
  scope.define_property(obj, closed_key, internal_data_desc(Value::Bool(false)))?;

  // Public properties.
  let public_name_key = alloc_key(scope, "name")?;
  scope
    .define_property(obj, public_name_key, data_desc(Value::String(name_s), false))?;

  let onmessage_key = alloc_key(scope, "onmessage")?;
  scope.define_property(obj, onmessage_key, data_desc(Value::Null, true))?;

  // Eagerly create the listeners array slot so we don't allocate during dispatch.
  let _ = get_or_create_listeners_array(vm, scope, obj)?;

  // Register in the global registry.
  let Some(realm_id) = vm.current_realm() else {
    return Err(VmError::InvariantViolation(
      "BroadcastChannel constructed without an active realm",
    ));
  };

  let entry = ChannelEntry {
    obj: WeakGcObject::new(obj),
    vm_ptr: vm as *mut Vm as usize,
    heap_ptr: scope.heap_mut() as *mut Heap as usize,
    realm_id,
    last_gc_runs: scope.heap().gc_runs(),
  };

  let key = RegistryKey { origin, name };

  let mut reg = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  let list = reg.channels.entry(key).or_default();
  if list.len() >= MAX_CHANNELS_PER_KEY {
    return Err(VmError::TypeError(
      "Too many BroadcastChannel instances for this channel name",
    ));
  }
  list.push(entry);

  Ok(Value::Object(obj))
}

fn broadcast_channel_add_event_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let channel_obj = require_channel(scope, this)?;
  if channel_is_closed(scope, channel_obj)? {
    return Err(VmError::TypeError(CLOSED_ERROR));
  }

  let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let type_s = scope.to_string(vm, host, hooks, type_arg)?;
  let type_name = scope
    .heap()
    .get_string(type_s)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();
  if type_name != "message" {
    return Ok(Value::Undefined);
  }

  let listener = args.get(1).copied().unwrap_or(Value::Undefined);
  let Value::Object(listener_obj) = listener else {
    // Per DOM, `null` listeners are no-ops.
    return Ok(Value::Undefined);
  };
  if !scope.heap().is_callable(Value::Object(listener_obj))? {
    return Ok(Value::Undefined);
  }

  let listeners_arr = get_or_create_listeners_array(vm, scope, channel_obj)?;
  let len = listeners_array_len(scope, listeners_arr)?;

  // Avoid unbounded growth even if `removeEventListener` isn't used.
  if len >= MAX_LISTENERS_PER_CHANNEL {
    return Err(VmError::TypeError("BroadcastChannel has too many listeners"));
  }

  // De-dupe.
  for i in 0..len {
    let key = alloc_key(scope, &i.to_string())?;
    let existing = scope
      .heap()
      .object_get_own_data_property_value(listeners_arr, &key)?
      .unwrap_or(Value::Undefined);
    if existing == listener {
      return Ok(Value::Undefined);
    }
  }

  let key = alloc_key(scope, &len.to_string())?;
  scope.push_root(listener)?;
  scope.define_property(
    listeners_arr,
    key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: listener,
        writable: true,
      },
    },
  )?;
  Ok(Value::Undefined)
}

fn broadcast_channel_remove_event_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let channel_obj = require_channel(scope, this)?;
  if channel_is_closed(scope, channel_obj)? {
    return Err(VmError::TypeError(CLOSED_ERROR));
  }

  let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let type_s = scope.to_string(vm, host, hooks, type_arg)?;
  let type_name = scope
    .heap()
    .get_string(type_s)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();
  if type_name != "message" {
    return Ok(Value::Undefined);
  }

  let listener = args.get(1).copied().unwrap_or(Value::Undefined);
  let Value::Object(_listener_obj) = listener else {
    return Ok(Value::Undefined);
  };

  let listeners_key = alloc_key(scope, INTERNAL_LISTENERS_KEY)?;
  let Some(Value::Object(listeners_arr)) =
    scope.heap().object_get_own_data_property_value(channel_obj, &listeners_key)?
  else {
    return Ok(Value::Undefined);
  };
  let len = listeners_array_len(scope, listeners_arr)?;
  for i in 0..len {
    let key = alloc_key(scope, &i.to_string())?;
    let existing = scope
      .heap()
      .object_get_own_data_property_value(listeners_arr, &key)?
      .unwrap_or(Value::Undefined);
    if existing == listener {
      // Leave a hole (set to undefined) so we don't have to compact the array.
      scope.define_property(listeners_arr, key, data_desc(Value::Undefined, true))?;
    }
  }

  Ok(Value::Undefined)
}

fn broadcast_channel_close_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let channel_obj = require_channel(scope, this)?;
  if channel_is_closed(scope, channel_obj)? {
    return Ok(Value::Undefined);
  }

  // Mark closed.
  scope.push_root(Value::Object(channel_obj))?;
  let closed_key = alloc_key(scope, INTERNAL_CLOSED_KEY)?;
  scope.define_property(channel_obj, closed_key, internal_data_desc(Value::Bool(true)))?;

  // Remove from registry.
  let key = channel_registry_key(scope, channel_obj)?;
  let weak = WeakGcObject::new(channel_obj);
  let mut reg = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  if let Some(list) = reg.channels.get_mut(&key) {
    list.retain(|entry| entry.obj != weak);
    if list.is_empty() {
      reg.channels.remove(&key);
    }
  }

  Ok(Value::Undefined)
}

fn broadcast_channel_post_message_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let channel_obj = require_channel(scope, this)?;
  if channel_is_closed(scope, channel_obj)? {
    return Err(VmError::TypeError(CLOSED_ERROR));
  }

  let key = channel_registry_key(scope, channel_obj)?;
  let origin = key.origin.clone();

  // Serialize the message once in the sender realm.
  let message_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut budget = SerializeBudget { bytes: 0, nodes: 0 };
  let serialized = serialize_value(vm, scope, message_value, 0, &mut budget)?;

  // Collect recipients without holding the registry lock during JS calls.
  let mut dead: Vec<(RegistryKey, WeakGcObject)> = Vec::new();
  let recipients: Vec<ChannelEntry> = {
    let mut reg = registry()
      .lock()
      .unwrap_or_else(|err| err.into_inner());
    let Some(list) = reg.channels.get_mut(&key) else {
      return Ok(Value::Undefined);
    };
    // Sweep dead entries opportunistically when GC has run.
    list.retain(|entry| {
      let heap = unsafe { &*(entry.heap_ptr as *const Heap) };
      let gc_runs = heap.gc_runs();
      if gc_runs != entry.last_gc_runs {
        // Update the entry's gc run counter.
        // (We can't mutate `entry` here; clone and patch below by retaining on upgrade status.)
        if entry.obj.upgrade(heap).is_none() {
          dead.push((key.clone(), entry.obj));
          return false;
        }
      }
      true
    });
    list.clone()
  };

  // Deliver to all other open channels of the same origin+name.
  let sender_weak = WeakGcObject::new(channel_obj);

  for entry in recipients {
    if entry.obj == sender_weak {
      continue;
    }

    // Determine whether this delivery targets the sender realm to avoid aliasing raw pointers.
    let same_vm = entry.vm_ptr == (vm as *mut Vm as usize);

    if same_vm {
      // Same realm: upgrade using the current heap.
      let Some(target_obj) = entry.obj.upgrade(scope.heap()) else {
        continue;
      };
      // Clone into the same realm (still uses structured clone semantics).
      let msg = deserialize_value(vm, scope, &serialized)?;
      dispatch_message_event(vm, scope, host, hooks, target_obj, msg, &origin)?;
      continue;
    }

    // Different realm: dispatch using raw pointers.
    // SAFETY: `ChannelEntry` is removed via `teardown_window_broadcast_channel_bindings_for_realm`
    // before the realm is dropped, so `vm`/`heap` remain valid while the entry is live.
    unsafe {
      let recv_vm = &mut *(entry.vm_ptr as *mut Vm);
      let recv_heap = &mut *(entry.heap_ptr as *mut Heap);
      let Some(target_obj) = entry.obj.upgrade(recv_heap) else {
        continue;
      };

      // Route promise jobs created during handler invocation into the receiver VM's microtask queue
      // to avoid leaking persistent roots.
      struct MicrotaskQueueHooks {
        microtasks: *mut vm_js::MicrotaskQueue,
      }
      impl VmHostHooks for MicrotaskQueueHooks {
        fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
          // SAFETY: `microtasks` points into the receiver `Vm`'s microtask queue. The receiver VM
          // outlives this hook (it is stack-scoped to the dispatch call).
          unsafe { (&mut *self.microtasks).enqueue_promise_job(job, realm) };
        }

        fn host_enqueue_promise_job_fallible(
          &mut self,
          ctx: &mut dyn vm_js::VmJobContext,
          job: vm_js::Job,
          realm: Option<vm_js::RealmId>,
        ) -> Result<(), VmError> {
          // SAFETY: `microtasks` points into the receiver VM's microtask queue (see above).
          unsafe {
            vm_js::VmHostHooks::host_enqueue_promise_job_fallible(&mut *self.microtasks, ctx, job, realm)
          }
        }
      }

      let mut recv_scope = recv_heap.scope();
      let mut recv_host = ();
      // Take a raw pointer so we don't hold an outstanding `&mut` borrow of `recv_vm` while also
      // running with an execution-context guard.
      let microtasks_ptr = recv_vm.microtask_queue_mut() as *mut vm_js::MicrotaskQueue;
      let mut recv_hooks = MicrotaskQueueHooks {
        microtasks: microtasks_ptr,
      };
      let mut recv_vm = recv_vm.execution_context_guard(vm_js::ExecutionContext {
        realm: entry.realm_id,
        script_or_module: None,
      })?;

      let msg = deserialize_value(&mut recv_vm, &mut recv_scope, &serialized)?;
      let _ = dispatch_message_event(
        &mut recv_vm,
        &mut recv_scope,
        &mut recv_host,
        &mut recv_hooks,
        target_obj,
        msg,
        &origin,
      );
    }
  }

  // Best-effort cleanup for dead entries discovered during sweep.
  if !dead.is_empty() {
    let mut reg = registry()
      .lock()
      .unwrap_or_else(|err| err.into_inner());
    for (key, obj) in dead {
      if let Some(list) = reg.channels.get_mut(&key) {
        list.retain(|e| e.obj != obj);
        if list.is_empty() {
          reg.channels.remove(&key);
        }
      }
    }
  }

  Ok(Value::Undefined)
}

pub fn install_window_broadcast_channel_bindings(vm: &mut Vm, realm: &Realm, heap: &mut Heap) -> Result<(), VmError> {
  let intr = realm.intrinsics();
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // BroadcastChannel.prototype
  let proto = scope.alloc_object()?;
  scope.push_root(Value::Object(proto))?;
  scope
    .heap_mut()
    .object_set_prototype(proto, Some(intr.object_prototype()))?;

  // Prototype methods.
  let func_proto = intr.function_prototype();

  let add_id = vm.register_native_call(broadcast_channel_add_event_listener_native)?;
  let add_name = scope.alloc_string("addEventListener")?;
  scope.push_root(Value::String(add_name))?;
  let add_fn = scope.alloc_native_function(add_id, None, add_name, 2)?;
  scope.heap_mut().object_set_prototype(add_fn, Some(func_proto))?;
  let add_key = alloc_key(&mut scope, "addEventListener")?;
  scope.define_property(proto, add_key, data_desc(Value::Object(add_fn), true))?;

  let remove_id = vm.register_native_call(broadcast_channel_remove_event_listener_native)?;
  let remove_name = scope.alloc_string("removeEventListener")?;
  scope.push_root(Value::String(remove_name))?;
  let remove_fn = scope.alloc_native_function(remove_id, None, remove_name, 2)?;
  scope.heap_mut().object_set_prototype(remove_fn, Some(func_proto))?;
  let remove_key = alloc_key(&mut scope, "removeEventListener")?;
  scope.define_property(proto, remove_key, data_desc(Value::Object(remove_fn), true))?;

  let close_id = vm.register_native_call(broadcast_channel_close_native)?;
  let close_name = scope.alloc_string("close")?;
  scope.push_root(Value::String(close_name))?;
  let close_fn = scope.alloc_native_function(close_id, None, close_name, 0)?;
  scope.heap_mut().object_set_prototype(close_fn, Some(func_proto))?;
  let close_key = alloc_key(&mut scope, "close")?;
  scope.define_property(proto, close_key, data_desc(Value::Object(close_fn), true))?;

  let post_id = vm.register_native_call(broadcast_channel_post_message_native)?;
  let post_name = scope.alloc_string("postMessage")?;
  scope.push_root(Value::String(post_name))?;
  let post_fn = scope.alloc_native_function(post_id, None, post_name, 1)?;
  scope.heap_mut().object_set_prototype(post_fn, Some(func_proto))?;
  let post_key = alloc_key(&mut scope, "postMessage")?;
  scope.define_property(proto, post_key, data_desc(Value::Object(post_fn), true))?;

  // Constructor.
  let call_id = vm.register_native_call(broadcast_channel_ctor_call)?;
  let construct_id = vm.register_native_construct(broadcast_channel_ctor_construct)?;
  let name = scope.alloc_string("BroadcastChannel")?;
  scope.push_root(Value::String(name))?;
  let ctor = scope.alloc_native_function(call_id, Some(construct_id), name, 1)?;
  scope.push_root(Value::Object(ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(ctor, Some(intr.function_prototype()))?;

  // Link constructor <-> prototype.
  let prototype_key = alloc_key(&mut scope, "prototype")?;
  let constructor_key = alloc_key(&mut scope, "constructor")?;
  scope.define_property(ctor, prototype_key, internal_data_desc(Value::Object(proto)))?;
  scope.define_property(proto, constructor_key, internal_data_desc(Value::Object(ctor)))?;

  // Expose on global.
  let ctor_key = alloc_key(&mut scope, "BroadcastChannel")?;
  scope.define_property(global, ctor_key, data_desc(Value::Object(ctor), true))?;

  Ok(())
}

pub fn teardown_window_broadcast_channel_bindings_for_realm(realm_id: RealmId) {
  let mut reg = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  reg.channels.retain(|_, entries| {
    entries.retain(|e| e.realm_id != realm_id);
    !entries.is_empty()
  });
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value, got {value:?}");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn broadcast_channel_is_exposed() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let ty = realm.exec_script("typeof BroadcastChannel")?;
    assert_eq!(get_string(realm.heap(), ty), "function");
    realm.teardown();
    Ok(())
  }

  #[test]
  fn broadcast_channel_same_origin_delivers_to_other_realm() -> Result<(), VmError> {
    let mut a = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let mut b = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    b.exec_script(
      "globalThis.__received = null;\n\
       globalThis.__bc = new BroadcastChannel('test');\n\
       __bc.addEventListener('message', (e) => { globalThis.__received = e.data; });",
    )?;

    a.exec_script("new BroadcastChannel('test').postMessage('hello')")?;

    let received = b.exec_script("__received")?;
    assert_eq!(get_string(b.heap(), received), "hello");

    a.teardown();
    b.teardown();
    Ok(())
  }

  #[test]
  fn broadcast_channel_different_origin_does_not_deliver() -> Result<(), VmError> {
    let mut a = WindowRealm::new(WindowRealmConfig::new("https://a.example/"))?;
    let mut b = WindowRealm::new(WindowRealmConfig::new("https://b.example/"))?;

    b.exec_script(
      "globalThis.__received = null;\n\
       globalThis.__bc = new BroadcastChannel('test');\n\
       __bc.onmessage = (e) => { globalThis.__received = e.data; };",
    )?;

    a.exec_script("new BroadcastChannel('test').postMessage('hello')")?;

    let received = b.exec_script("__received")?;
    assert!(matches!(received, Value::Null));

    a.teardown();
    b.teardown();
    Ok(())
  }

  #[test]
  fn broadcast_channel_close_stops_delivery() -> Result<(), VmError> {
    let mut a = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let mut b = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    b.exec_script(
      "globalThis.__count = 0;\n\
       globalThis.__bc = new BroadcastChannel('test');\n\
       __bc.onmessage = () => { globalThis.__count++; };",
    )?;

    a.exec_script("globalThis.__a = new BroadcastChannel('test'); __a.postMessage('x');")?;
    assert_eq!(b.exec_script("__count")?, Value::Number(1.0));

    b.exec_script("__bc.close()")?;
    a.exec_script("__a.postMessage('y');")?;
    assert_eq!(b.exec_script("__count")?, Value::Number(1.0));

    a.teardown();
    b.teardown();
    Ok(())
  }
}
