//! Minimal `Blob` implementation for `vm-js` Window realms.
//!
//! This is a spec-shaped MVP intended to unblock real-world scripts that expect `Blob` to exist and
//! to allow `fetch()` request bodies to accept `Blob` objects.
//!
//! Supported BlobParts:
//! - `string` (UTF-8 encoded, replacing invalid surrogates)
//! - `ArrayBuffer`
//! - `Uint8Array`
//! - `Blob`
//!
//! Storage is host-side and bounded to avoid untrusted-script OOM/DoS.

use std::char::decode_utf16;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use vm_js::{
  new_promise_capability_with_host_and_hooks, GcObject, GcString, Heap, NativeConstructId,
  NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind, Realm, RealmId, Scope, Value,
  Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};

const BLOB_CTOR_REALM_ID_SLOT: usize = 0;

/// Hard upper bound on a single Blob's byte length.
///
/// This is intentionally kept <= `WebFetchLimits::max_request_body_bytes` (10MiB) so `Blob` can be
/// used as a `fetch()` request body without additional surprising size limits.
pub(crate) const MAX_BLOB_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct BlobData {
  pub(crate) bytes: Vec<u8>,
  pub(crate) r#type: String,
}

#[derive(Default)]
struct BlobRegistry {
  realms: HashMap<RealmId, BlobRealmState>,
}

struct BlobRealmState {
  blob_proto: GcObject,
  blobs: HashMap<WeakGcObject, BlobData>,
  last_gc_runs: u64,
}

static REGISTRY: OnceLock<Mutex<BlobRegistry>> = OnceLock::new();

fn registry() -> &'static Mutex<BlobRegistry> {
  REGISTRY.get_or_init(|| Mutex::new(BlobRegistry::default()))
}

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

fn realm_id_from_slot(value: Value) -> Option<RealmId> {
  let Value::Number(n) = value else {
    return None;
  };
  if !n.is_finite() || n < 0.0 {
    return None;
  }
  let raw = n as u64;
  if raw as f64 != n {
    return None;
  }
  Some(RealmId::from_raw(raw))
}

fn realm_id_for_binding_call(
  vm: &Vm,
  scope: &Scope<'_>,
  callee: GcObject,
) -> Result<RealmId, VmError> {
  if let Some(realm_id) = vm.current_realm() {
    return Ok(realm_id);
  }

  let slots = scope.heap().get_function_native_slots(callee)?;
  let realm_id = slots
    .get(BLOB_CTOR_REALM_ID_SLOT)
    .copied()
    .and_then(realm_id_from_slot)
    .ok_or(VmError::InvariantViolation(
      "Blob bindings invoked without an active realm",
    ))?;
  Ok(realm_id)
}

fn with_realm_state_mut<R>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  f: impl FnOnce(&mut BlobRealmState) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let realm_id = realm_id_for_binding_call(vm, scope, callee)?;

  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  let state = registry
    .realms
    .get_mut(&realm_id)
    .ok_or(VmError::InvariantViolation(
      "Blob bindings used before install_window_blob_bindings",
    ))?;

  // Opportunistically sweep dead Blobs when GC has run.
  let gc_runs = scope.heap().gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    let heap = scope.heap();
    state.blobs.retain(|k, _| k.upgrade(heap).is_some());
  }

  f(state)
}

fn require_blob<'a>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  this: Value,
) -> Result<(GcObject, BlobData), VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Blob: illegal invocation"));
  };

  let data = with_realm_state_mut(vm, scope, callee, |state| {
    state
      .blobs
      .get(&WeakGcObject::from(obj))
      .cloned()
      .ok_or(VmError::TypeError("Blob: illegal invocation"))
  })?;

  Ok((obj, data))
}

pub(crate) fn normalize_type(s: &str) -> String {
  if s.is_empty() {
    return String::new();
  }

  // File API: type is ASCII-lowercased, and set to empty if it contains non-ASCII-printable bytes.
  if !s
    .as_bytes()
    .iter()
    .copied()
    .all(|b| (0x20..=0x7E).contains(&b))
  {
    return String::new();
  }

  s.bytes()
    .map(|b| (b as char).to_ascii_lowercase())
    .collect()
}

fn js_string_to_utf8_bytes_limited(
  heap: &Heap,
  s: GcString,
  out: &mut Vec<u8>,
) -> Result<(), VmError> {
  let units = heap.get_string(s)?.as_code_units();
  for decoded in decode_utf16(units.iter().copied()) {
    let ch = decoded.unwrap_or('\u{FFFD}');
    let mut buf = [0u8; 4];
    let encoded = ch.encode_utf8(&mut buf);
    let next_len = out
      .len()
      .checked_add(encoded.len())
      .ok_or(VmError::OutOfMemory)?;
    if next_len > MAX_BLOB_BYTES {
      return Err(VmError::TypeError("Blob size exceeds maximum length"));
    }
    out.extend_from_slice(encoded.as_bytes());
  }
  Ok(())
}

fn append_bytes_limited(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), VmError> {
  let next_len = out
    .len()
    .checked_add(bytes.len())
    .ok_or(VmError::OutOfMemory)?;
  if next_len > MAX_BLOB_BYTES {
    return Err(VmError::TypeError("Blob size exceeds maximum length"));
  }
  out
    .try_reserve_exact(bytes.len())
    .map_err(|_| VmError::OutOfMemory)?;
  out.extend_from_slice(bytes);
  Ok(())
}

fn blob_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("Blob constructor requires 'new'"))
}

fn blob_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "Blob requires intrinsics (create a Realm first)",
  ))?;

  // Collect the parts up front without holding any registry lock: property access and ToString can
  // invoke user code.
  let parts_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut parts: Vec<Value> = Vec::new();

  if !matches!(parts_val, Value::Undefined | Value::Null) {
    if let Value::Object(parts_obj) = parts_val {
      if scope.heap().object_prototype(parts_obj)? == Some(intr.array_prototype()) {
        let length_key = alloc_key(scope, "length")?;
        let len_val = vm.get_with_host_and_hooks(host, scope, hooks, parts_obj, length_key)?;
        let Value::Number(n) = len_val else {
          return Err(VmError::TypeError("Blob parts array has invalid length"));
        };
        if !n.is_finite() || n < 0.0 || n > (u32::MAX as f64) {
          return Err(VmError::TypeError("Blob parts array has invalid length"));
        }
        let len = n as usize;
        parts
          .try_reserve_exact(len)
          .map_err(|_| VmError::OutOfMemory)?;
        for i in 0..len {
          let key = alloc_key(scope, &i.to_string())?;
          let v = vm.get_with_host_and_hooks(host, scope, hooks, parts_obj, key)?;
          parts.push(v);
        }
      } else {
        parts.push(parts_val);
      }
    } else {
      parts.push(parts_val);
    }
  }

  // Parse options: { type }.
  let mut type_string = String::new();
  let options_val = args.get(1).copied().unwrap_or(Value::Undefined);
  if !matches!(options_val, Value::Undefined | Value::Null) {
    let Value::Object(options_obj) = options_val else {
      return Err(VmError::TypeError("Blob options must be an object"));
    };
    let type_key = alloc_key(scope, "type")?;
    let type_val = vm.get_with_host_and_hooks(host, scope, hooks, options_obj, type_key)?;
    if !matches!(type_val, Value::Undefined) {
      let s = scope.to_string(vm, host, hooks, type_val)?;
      type_string = scope.heap().get_string(s)?.to_utf8_lossy();
    }
  }
  let type_string = normalize_type(&type_string);

  let mut bytes: Vec<u8> = Vec::new();
  for part in parts {
    if let Value::Object(obj) = part {
      if let Ok(Some(blob)) = clone_blob_data_for_object(vm, scope.heap(), obj) {
        append_bytes_limited(&mut bytes, &blob.bytes)?;
        continue;
      }
      if scope.heap().is_array_buffer_object(obj) {
        let data = scope.heap().array_buffer_data(obj)?;
        append_bytes_limited(&mut bytes, data)?;
        continue;
      }
      if scope.heap().is_uint8_array_object(obj) {
        let data = scope.heap().uint8_array_data(obj)?;
        append_bytes_limited(&mut bytes, data)?;
        continue;
      }
    }

    let s = scope.to_string(vm, host, hooks, part)?;
    js_string_to_utf8_bytes_limited(scope.heap(), s, &mut bytes)?;
  }

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(callee))?;

  let proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(proto) => proto,
      _ => intr.object_prototype(),
    }
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  // Expose read-only `size` and `type` instance properties (common real-world usage).
  let size_key = alloc_key(&mut scope, "size")?;
  scope.define_property(
    obj,
    size_key,
    data_desc(Value::Number(bytes.len() as f64), false),
  )?;

  let type_key = alloc_key(&mut scope, "type")?;
  let type_js = scope.alloc_string(&type_string)?;
  scope.push_root(Value::String(type_js))?;
  scope.define_property(obj, type_key, data_desc(Value::String(type_js), false))?;

  with_realm_state_mut(vm, &mut scope, callee, |state| {
    state.blobs.insert(
      WeakGcObject::from(obj),
      BlobData {
        bytes,
        r#type: type_string,
      },
    );
    Ok(())
  })?;

  Ok(Value::Object(obj))
}

fn clone_blob_data_for_object(
  vm: &Vm,
  heap: &Heap,
  obj: GcObject,
) -> Result<Option<BlobData>, VmError> {
  let Some(realm_id) = vm.current_realm() else {
    return Ok(None);
  };
  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  let Some(state) = registry.realms.get_mut(&realm_id) else {
    return Ok(None);
  };

  // Opportunistic sweep.
  let gc_runs = heap.gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    state.blobs.retain(|k, _| k.upgrade(heap).is_some());
  }

  Ok(state.blobs.get(&WeakGcObject::from(obj)).cloned())
}

pub(crate) fn clone_blob_data_for_fetch(
  vm: &Vm,
  heap: &Heap,
  value: Value,
) -> Result<Option<BlobData>, VmError> {
  let Value::Object(obj) = value else {
    return Ok(None);
  };
  clone_blob_data_for_object(vm, heap, obj)
}

pub(crate) fn blob_prototype_for_realm(realm_id: RealmId) -> Option<GcObject> {
  let registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  registry.realms.get(&realm_id).map(|s| s.blob_proto)
}

pub(crate) fn create_blob_with_proto(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  proto: GcObject,
  data: BlobData,
) -> Result<GcObject, VmError> {
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  let size_key = alloc_key(scope, "size")?;
  scope.define_property(
    obj,
    size_key,
    data_desc(Value::Number(data.bytes.len() as f64), false),
  )?;

  let type_key = alloc_key(scope, "type")?;
  let type_js = scope.alloc_string(&data.r#type)?;
  scope.push_root(Value::String(type_js))?;
  scope.define_property(obj, type_key, data_desc(Value::String(type_js), false))?;

  with_realm_state_mut(vm, scope, callee, |state| {
    state.blobs.insert(WeakGcObject::from(obj), data);
    Ok(())
  })?;

  Ok(obj)
}

fn to_integer_or_default(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  default: i64,
) -> Result<i64, VmError> {
  if matches!(value, Value::Undefined) {
    return Ok(default);
  }
  let n = scope.to_number(vm, host, hooks, value)?;
  if n.is_nan() {
    return Ok(0);
  }
  if n == f64::INFINITY {
    return Ok(i64::MAX);
  }
  if n == f64::NEG_INFINITY {
    return Ok(i64::MIN);
  }
  Ok(n.trunc() as i64)
}

fn blob_slice_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (_this_obj, data) = require_blob(vm, scope, callee, this)?;

  let size = data.bytes.len() as i64;
  let start = to_integer_or_default(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    0,
  )?;
  let end = to_integer_or_default(
    vm,
    scope,
    host,
    hooks,
    args.get(1).copied().unwrap_or(Value::Undefined),
    size,
  )?;

  let relative_start = if start < 0 {
    size.saturating_add(start).max(0)
  } else {
    start.min(size)
  };
  let relative_end = if end < 0 {
    size.saturating_add(end).max(0)
  } else {
    end.min(size)
  };
  let span = (relative_end - relative_start).max(0) as usize;
  let offset = relative_start as usize;
  let bytes = data
    .bytes
    .get(offset..offset.saturating_add(span))
    .unwrap_or(&[])
    .to_vec();

  let mut content_type = String::new();
  if let Some(v) = args.get(2).copied() {
    if !matches!(v, Value::Undefined) {
      let s = scope.to_string(vm, host, hooks, v)?;
      content_type = scope.heap().get_string(s)?.to_utf8_lossy();
    }
  }
  let content_type = normalize_type(&content_type);

  // Spec behavior: `Blob.prototype.slice` always returns a `Blob` instance (even when invoked on
  // subclasses like `File`), so we always construct with this realm's `Blob.prototype`.
  let proto = with_realm_state_mut(vm, scope, callee, |state| Ok(state.blob_proto))?;

  let blob = create_blob_with_proto(
    vm,
    scope,
    callee,
    proto,
    BlobData {
      bytes,
      r#type: content_type,
    },
  )?;
  Ok(Value::Object(blob))
}

fn blob_text_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (_obj, data) = require_blob(vm, scope, callee, this)?;

  let cap = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = scope.push_root(cap.promise)?;
  let resolve = scope.push_root(cap.resolve)?;

  let text = String::from_utf8_lossy(&data.bytes);
  let s = scope.alloc_string(&text)?;
  scope.push_root(Value::String(s))?;
  vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    resolve,
    Value::Undefined,
    &[Value::String(s)],
  )?;

  Ok(promise)
}

fn blob_array_buffer_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (_obj, data) = require_blob(vm, scope, callee, this)?;
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "Blob.arrayBuffer requires intrinsics (create a Realm first)",
  ))?;

  let cap = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = scope.push_root(cap.promise)?;
  let resolve = scope.push_root(cap.resolve)?;

  let ab = scope.alloc_array_buffer_from_u8_vec(data.bytes)?;
  scope.push_root(Value::Object(ab))?;
  scope
    .heap_mut()
    .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;

  vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    resolve,
    Value::Undefined,
    &[Value::Object(ab)],
  )?;
  Ok(promise)
}

pub fn install_window_blob_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
) -> Result<(), VmError> {
  let intr = realm.intrinsics();
  let realm_id = realm.id();

  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let call_id: NativeFunctionId = vm.register_native_call(blob_ctor_call)?;
  let construct_id: NativeConstructId = vm.register_native_construct(blob_ctor_construct)?;

  let name = scope.alloc_string("Blob")?;
  scope.push_root(Value::String(name))?;
  let ctor = scope.alloc_native_function_with_slots(
    call_id,
    Some(construct_id),
    name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(ctor, Some(intr.function_prototype()))?;

  let proto = {
    let key = alloc_key(&mut scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(ctor, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "Blob constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(proto))?;
  scope
    .heap_mut()
    .object_set_prototype(proto, Some(intr.object_prototype()))?;

  let slice_call_id: NativeFunctionId = vm.register_native_call(blob_slice_native)?;
  let slice_name = scope.alloc_string("slice")?;
  scope.push_root(Value::String(slice_name))?;
  let slice_fn = scope.alloc_native_function_with_slots(
    slice_call_id,
    None,
    slice_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(slice_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(slice_fn, Some(intr.function_prototype()))?;
  let slice_key = alloc_key(&mut scope, "slice")?;
  scope.define_property(proto, slice_key, data_desc(Value::Object(slice_fn), true))?;

  let text_call_id: NativeFunctionId = vm.register_native_call(blob_text_native)?;
  let text_name = scope.alloc_string("text")?;
  scope.push_root(Value::String(text_name))?;
  let text_fn = scope.alloc_native_function_with_slots(
    text_call_id,
    None,
    text_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(text_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(text_fn, Some(intr.function_prototype()))?;
  let text_key = alloc_key(&mut scope, "text")?;
  scope.define_property(proto, text_key, data_desc(Value::Object(text_fn), true))?;

  let ab_call_id: NativeFunctionId = vm.register_native_call(blob_array_buffer_native)?;
  let ab_name = scope.alloc_string("arrayBuffer")?;
  scope.push_root(Value::String(ab_name))?;
  let ab_fn = scope.alloc_native_function_with_slots(
    ab_call_id,
    None,
    ab_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(ab_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(ab_fn, Some(intr.function_prototype()))?;
  let ab_key = alloc_key(&mut scope, "arrayBuffer")?;
  scope.define_property(proto, ab_key, data_desc(Value::Object(ab_fn), true))?;

  let to_string_tag = intr.well_known_symbols().to_string_tag;
  let tag_key = PropertyKey::from_symbol(to_string_tag);
  let tag_value = scope.alloc_string("Blob")?;
  scope.push_root(Value::String(tag_value))?;
  scope.define_property(proto, tag_key, data_desc(Value::String(tag_value), false))?;

  let ctor_key = alloc_key(&mut scope, "Blob")?;
  scope.define_property(global, ctor_key, data_desc(Value::Object(ctor), true))?;

  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  registry.realms.insert(
    realm_id,
    BlobRealmState {
      blob_proto: proto,
      blobs: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
    },
  );

  Ok(())
}

pub fn teardown_window_blob_bindings_for_realm(realm_id: RealmId) {
  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  registry.realms.remove(&realm_id);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use vm_js::PromiseState;

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value, got {value:?}");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn blob_size_and_type() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let size = realm.exec_script("new Blob(['hi'], { type: 'Text/Plain' }).size")?;
    assert_eq!(size, Value::Number(2.0));

    let ty = realm.exec_script("new Blob(['hi'], { type: 'Text/Plain' }).type")?;
    assert_eq!(get_string(realm.heap(), ty), "text/plain");

    // Non-ASCII types are clamped to empty string.
    let ty2 = realm.exec_script("new Blob(['hi'], { type: 'text/plain\\u00FF' }).type")?;
    assert_eq!(get_string(realm.heap(), ty2), "");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn blob_slice_clamps_and_handles_negative_offsets() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let v = realm.exec_script("new Blob(['hello']).slice(1, 4).size")?;
    assert_eq!(v, Value::Number(3.0));

    let promise = realm.exec_script("new Blob(['hello']).slice(-2).text()")?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation(
        "Blob.text must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = realm.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation(
        "Blob.text promise missing result",
      ));
    };
    assert_eq!(get_string(realm.heap(), result), "lo");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn blob_text_and_array_buffer_match_bytes() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let promise = realm.exec_script("new Blob(['h', 'i']).text()")?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation(
        "Blob.text must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = realm.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation(
        "Blob.text promise missing result",
      ));
    };
    assert_eq!(get_string(realm.heap(), result), "hi");

    let promise = realm.exec_script("new Blob(['hi']).arrayBuffer()")?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation(
        "Blob.arrayBuffer must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = realm.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation(
        "Blob.arrayBuffer promise missing result",
      ));
    };
    let Value::Object(ab_obj) = result else {
      return Err(VmError::InvariantViolation(
        "Blob.arrayBuffer must resolve to an ArrayBuffer object",
      ));
    };
    assert!(realm.heap().is_array_buffer_object(ab_obj));
    assert_eq!(realm.heap().array_buffer_data(ab_obj)?, b"hi");

    realm.teardown();
    Ok(())
  }
}
