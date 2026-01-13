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

use crate::js::window_streams;
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

/// Hard upper bound on `Blob` type strings (`BlobPropertyBag.type`, `Blob.prototype.slice(..., contentType)`).
///
/// Real-world MIME type strings are tiny (usually <100 bytes). We cap these strings to keep host-side
/// allocations deterministic and to prevent untrusted scripts from forcing large Rust allocations via
/// extremely large `type`/`contentType` values.
const MAX_BLOB_TYPE_BYTES: usize = 4096;

/// Hard upper bound on the number of Blob parts accepted by the constructor when given an array.
///
/// This bounds worst-case CPU work (iterating array indices) and stack rooting growth. Real-world
/// Blob construction typically uses a small number of parts (often a handful of strings/typed
/// arrays), so this limit is intentionally generous while still preventing pathological behavior.
const MAX_BLOB_PARTS: usize = 64 * 1024;

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

fn proto_data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn accessor_desc(get: Value, set: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Accessor { get, set },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn usize_to_str_buf(buf: &mut [u8; 24], mut n: usize) -> &str {
  // Enough for `usize::MAX` (20 digits on 64-bit), plus slack.
  let mut start = buf.len();
  if n == 0 {
    start -= 1;
    buf[start] = b'0';
  } else {
    while n != 0 {
      start -= 1;
      buf[start] = b'0' + ((n % 10) as u8);
      n /= 10;
    }
  }

  // Safety: we only write ASCII digits, which are valid UTF-8.
  unsafe { std::str::from_utf8_unchecked(&buf[start..]) }
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

fn blob_size_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Blob: illegal invocation"));
  };

  let len = with_realm_state_mut(vm, scope, callee, |state| {
    state
      .blobs
      .get(&WeakGcObject::from(obj))
      .map(|data| data.bytes.len())
      .ok_or(VmError::TypeError("Blob: illegal invocation"))
  })?;

  Ok(Value::Number(len as f64))
}

fn blob_type_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Blob: illegal invocation"));
  };

  let r#type = with_realm_state_mut(vm, scope, callee, |state| {
    state
      .blobs
      .get(&WeakGcObject::from(obj))
      .map(|data| data.r#type.clone())
      .ok_or(VmError::TypeError("Blob: illegal invocation"))
  })?;

  let ty = scope.alloc_string(&r#type)?;
  Ok(Value::String(ty))
}

pub(crate) fn normalize_type(s: &str) -> String {
  if s.is_empty() {
    return String::new();
  }

  // Keep host allocations deterministic: any oversized type is treated as invalid.
  if s.len() > MAX_BLOB_TYPE_BYTES {
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

fn js_string_to_rust_string_bounded_for_blob_type(
  heap: &Heap,
  handle: GcString,
) -> Result<Option<String>, VmError> {
  let js = heap.get_string(handle)?;

  let code_units_len = js.len_code_units();
  // UTF-8 output bytes are always >= UTF-16 code unit length (and can grow by up to 3 bytes per code
  // unit when decoding lone surrogates as U+FFFD). Reject overly large strings up-front to prevent
  // unbounded host allocations.
  if code_units_len > MAX_BLOB_TYPE_BYTES {
    return Ok(None);
  }

  // Decode manually so we can enforce the byte limit without relying on potentially-large
  // allocations performed by `String::from_utf16_lossy` / `to_utf8_lossy`.
  let capacity = code_units_len.saturating_mul(3).min(MAX_BLOB_TYPE_BYTES);
  let mut out = String::with_capacity(capacity);
  let mut out_len = 0usize;

  for decoded in decode_utf16(js.as_code_units().iter().copied()) {
    let ch = decoded.unwrap_or('\u{FFFD}');
    let ch_len = ch.len_utf8();
    let next_len = out_len.checked_add(ch_len).unwrap_or(usize::MAX);
    if next_len > MAX_BLOB_TYPE_BYTES {
      return Ok(None);
    }
    out.push(ch);
    out_len = next_len;
  }

  Ok(Some(out))
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
    out
      .try_reserve(encoded.len())
      .map_err(|_| VmError::OutOfMemory)?;
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
  new_target: Value,
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
        if len > MAX_BLOB_PARTS {
          return Err(VmError::TypeError("Blob parts array exceeds maximum length"));
        }
        parts
          .try_reserve_exact(len)
          .map_err(|_| VmError::OutOfMemory)?;
        let mut index_buf = [0u8; 24];
        for i in 0..len {
          let key = alloc_key(scope, usize_to_str_buf(&mut index_buf, i))?;
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

  // GC safety: parts can be produced by getters/accessors and become unreachable in JS immediately
  // after property access. We must root any stored Values across subsequent allocations (e.g.
  // parsing `options.type`) so they cannot be collected before we consume them.
  scope.push_roots(&parts)?;

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
      type_string =
        js_string_to_rust_string_bounded_for_blob_type(scope.heap(), s)?.unwrap_or_default();
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

      // BufferSource parts (ArrayBuffer / ArrayBufferView).
      //
      // Real browsers treat detached and out-of-bounds views as empty (byteLength === 0) rather
      // than throwing. Be similarly forgiving so transfer-list patterns like:
      // `structuredClone(buf, { transfer: [buf] }); new Blob([buf])`
      // do not unexpectedly throw.
      if scope.heap().is_array_buffer_object(obj) {
        if scope.heap().is_detached_array_buffer(obj)? {
          continue;
        }
        let data = scope.heap().array_buffer_data(obj)?;
        append_bytes_limited(&mut bytes, data)?;
        continue;
      }
      if scope.heap().is_uint8_array_object(obj) {
        // Use non-throwing typed-array introspection helpers so detached/out-of-bounds views are
        // treated as empty rather than throwing.
        let byte_len = scope.heap().typed_array_byte_length(obj)?;
        if byte_len == 0 {
          continue;
        }
        let byte_offset = scope.heap().typed_array_byte_offset(obj)?;
        let buf = scope.heap().typed_array_buffer(obj)?;
        // Detached backing buffers should be treated as empty for Blob construction.
        if scope.heap().is_detached_array_buffer(buf)? {
          continue;
        }
        let data = scope.heap().array_buffer_data(buf)?;
        let Some(end) = byte_offset.checked_add(byte_len) else {
          return Err(VmError::OutOfMemory);
        };
        let Some(slice) = data.get(byte_offset..end) else {
          continue;
        };
        append_bytes_limited(&mut bytes, slice)?;
        continue;
      }
    }

    let s = scope.to_string(vm, host, hooks, part)?;
    js_string_to_utf8_bytes_limited(scope.heap(), s, &mut bytes)?;
  }

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(callee))?;

  let new_target_obj = match new_target {
    Value::Object(obj) => {
      scope.push_root(Value::Object(obj))?;
      Some(obj)
    }
    _ => None,
  };

  let prototype_key = alloc_key(&mut scope, "prototype")?;

  // Spec-shaped behavior: honor `newTarget` for subclassing:
  // https://webidl.spec.whatwg.org/#dfn-get-prototype-from-constructor
  //
  // Accessing `newTarget.prototype` can invoke user code (Proxy traps/getters), so it must happen
  // before any per-realm Blob registry lock is held.
  let new_target_proto = if let Some(new_target_obj) = new_target_obj {
    match vm.get_with_host_and_hooks(host, &mut scope, hooks, new_target_obj, prototype_key)? {
      Value::Object(proto) => {
        scope.push_root(Value::Object(proto))?;
        Some(proto)
      }
      _ => None,
    }
  } else {
    None
  };

  // If `newTarget.prototype` isn't an object, fall back to this realm's intrinsic Blob prototype.
  let realm_id = realm_id_for_binding_call(vm, &scope, callee)?;
  let fallback_proto = if let Some(proto) = blob_prototype_for_realm(realm_id) {
    proto
  } else {
    // Minimal fallback: use `callee.prototype` if it's an object (should usually be `Blob.prototype`
    // unless user code clobbers it), otherwise `Object.prototype`.
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &prototype_key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(proto) => proto,
      _ => intr.object_prototype(),
    }
  };

  let proto = new_target_proto.unwrap_or(fallback_proto);
  scope.push_root(Value::Object(proto))?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  with_realm_state_mut(vm, &mut scope, callee, |state| {
    state
      .blobs
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
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

/// Create a `Blob` instance in the provided realm, storing its backing bytes in the host-side blob
/// registry.
///
/// This helper is intended for native code paths (e.g. `WebSocket` binary message dispatch) that
/// need to allocate a `Blob` without relying on an active VM execution context.
pub(crate) fn create_blob_for_realm(
  scope: &mut Scope<'_>,
  realm_id: RealmId,
  data: BlobData,
) -> Result<GcObject, VmError> {
  if data.bytes.len() > MAX_BLOB_BYTES {
    return Err(VmError::TypeError("Blob size exceeds maximum length"));
  }

  // Look up the realm's `Blob.prototype` and opportunistically sweep dead entries. Avoid holding
  // the registry lock while allocating on the JS heap.
  let proto = {
    let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
    let state = registry
      .realms
      .get_mut(&realm_id)
      .ok_or(VmError::Unimplemented("Blob bindings not installed"))?;

    let gc_runs = scope.heap().gc_runs();
    if gc_runs != state.last_gc_runs {
      state.last_gc_runs = gc_runs;
      let heap = scope.heap();
      state.blobs.retain(|k, _| k.upgrade(heap).is_some());
    }

    state.blob_proto
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  {
    let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
    let state = registry
      .realms
      .get_mut(&realm_id)
      .ok_or(VmError::Unimplemented("Blob bindings not installed"))?;
    state.blobs.insert(WeakGcObject::from(obj), data);
  }

  Ok(obj)
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

  with_realm_state_mut(vm, scope, callee, |state| {
    state
      .blobs
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
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
  let src = data
    .bytes
    .get(offset..offset.saturating_add(span))
    .unwrap_or(&[]);
  let mut bytes = Vec::new();
  bytes
    .try_reserve_exact(span)
    .map_err(|_| VmError::OutOfMemory)?;
  bytes.extend_from_slice(src);

  let mut content_type = String::new();
  if let Some(v) = args.get(2).copied() {
    if !matches!(v, Value::Undefined) {
      let s = scope.to_string(vm, host, hooks, v)?;
      content_type =
        js_string_to_rust_string_bounded_for_blob_type(scope.heap(), s)?.unwrap_or_default();
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

fn blob_stream_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (_obj, data) = require_blob(vm, scope, callee, this)?;
  let stream = window_streams::create_readable_byte_stream_from_bytes(vm, scope, callee, data.bytes)?;
  Ok(Value::Object(stream))
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

  let size_get_call_id: NativeFunctionId = vm.register_native_call(blob_size_get_native)?;
  let size_get_name = scope.alloc_string("get size")?;
  scope.push_root(Value::String(size_get_name))?;
  let size_get_fn = scope.alloc_native_function_with_slots(
    size_get_call_id,
    None,
    size_get_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(size_get_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(size_get_fn, Some(intr.function_prototype()))?;
  let size_key = alloc_key(&mut scope, "size")?;
  scope.define_property(
    proto,
    size_key,
    accessor_desc(Value::Object(size_get_fn), Value::Undefined),
  )?;

  let type_get_call_id: NativeFunctionId = vm.register_native_call(blob_type_get_native)?;
  let type_get_name = scope.alloc_string("get type")?;
  scope.push_root(Value::String(type_get_name))?;
  let type_get_fn = scope.alloc_native_function_with_slots(
    type_get_call_id,
    None,
    type_get_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(type_get_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(type_get_fn, Some(intr.function_prototype()))?;
  let type_key = alloc_key(&mut scope, "type")?;
  scope.define_property(
    proto,
    type_key,
    accessor_desc(Value::Object(type_get_fn), Value::Undefined),
  )?;

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
  scope.define_property(proto, slice_key, proto_data_desc(Value::Object(slice_fn), true))?;

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
  scope.define_property(proto, text_key, proto_data_desc(Value::Object(text_fn), true))?;

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
  scope.define_property(proto, ab_key, proto_data_desc(Value::Object(ab_fn), true))?;

  let stream_call_id: NativeFunctionId = vm.register_native_call(blob_stream_native)?;
  let stream_name = scope.alloc_string("stream")?;
  scope.push_root(Value::String(stream_name))?;
  let stream_fn = scope.alloc_native_function_with_slots(
    stream_call_id,
    None,
    stream_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(stream_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(stream_fn))?;
  let stream_key = alloc_key(&mut scope, "stream")?;
  scope.define_property(proto, stream_key, proto_data_desc(Value::Object(stream_fn), true))?;

  let to_string_tag = intr.well_known_symbols().to_string_tag;
  let tag_key = PropertyKey::from_symbol(to_string_tag);
  let tag_value = scope.alloc_string("Blob")?;
  scope.push_root(Value::String(tag_value))?;
  scope.define_property(proto, tag_key, data_desc(Value::String(tag_value), false))?;

  let ctor_key = alloc_key(&mut scope, "Blob")?;
  scope.define_property(global, ctor_key, data_desc(Value::Object(ctor), true))?;

  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  registry
    .realms
    .try_reserve(1)
    .map_err(|_| VmError::OutOfMemory)?;
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
  use vm_js::{HeapLimits, PromiseState};

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value, got {value:?}");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  fn read_result_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<Value, VmError> {
    let key = alloc_key(scope, name)?;
    Ok(
      scope
        .heap()
        .object_get_own_data_property_value(obj, &key)?
        .unwrap_or(Value::Undefined),
    )
  }

  #[test]
  fn blob_size_and_type() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let v = realm.exec_script("Object.getOwnPropertyDescriptor(new Blob(['x']), 'size') === undefined")?;
    assert_eq!(v, Value::Bool(true));
    let v = realm.exec_script("typeof Object.getOwnPropertyDescriptor(Blob.prototype, 'size').get === 'function'")?;
    assert_eq!(v, Value::Bool(true));

    let v = realm.exec_script("Object.getOwnPropertyDescriptor(new Blob(['x']), 'type') === undefined")?;
    assert_eq!(v, Value::Bool(true));
    let v = realm.exec_script("typeof Object.getOwnPropertyDescriptor(Blob.prototype, 'type').get === 'function'")?;
    assert_eq!(v, Value::Bool(true));

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
  fn file_inherits_blob_size_and_type() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let v = realm.exec_script("new File(['hi'], 'x.txt', { type: 'text/plain' }).size")?;
    assert_eq!(v, Value::Number(2.0));
    let v = realm.exec_script("new File(['hi'], 'x.txt', { type: 'text/plain' }).type")?;
    assert_eq!(get_string(realm.heap(), v), "text/plain");

    let v = realm.exec_script(
      "Object.getOwnPropertyDescriptor(new File(['hi'], 'x.txt', { type: 'text/plain' }), 'size') === undefined",
    )?;
    assert_eq!(v, Value::Bool(true));
    let v = realm.exec_script(
      "Object.getOwnPropertyDescriptor(new File(['hi'], 'x.txt', { type: 'text/plain' }), 'type') === undefined",
    )?;
    assert_eq!(v, Value::Bool(true));

    realm.teardown();
    Ok(())
  }

  #[test]
  fn blob_stream_returns_readable_stream_of_bytes() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    // Keep the reader alive across multiple `exec_script` calls.
    let _ = realm.exec_script("globalThis.reader = new Blob(['hi']).stream().getReader();")?;

    let p1 = realm.exec_script("reader.read()")?;
    let Value::Object(p1_obj) = p1 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(realm.heap().promise_state(p1_obj)?, PromiseState::Fulfilled);
    let Some(result1_val) = realm.heap().promise_result(p1_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Value::Object(result1_obj) = result1_val else {
      return Err(VmError::InvariantViolation("read() must resolve to an object"));
    };

    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done1 = read_result_prop(&mut scope, result1_obj, "done")?;
      assert_eq!(done1, Value::Bool(false));
      let value1 = read_result_prop(&mut scope, result1_obj, "value")?;
      let Value::Object(value1_obj) = value1 else {
        return Err(VmError::InvariantViolation(
          "read() result.value must be an object",
        ));
      };

      if scope.heap().is_uint8_array_object(value1_obj) {
        assert_eq!(scope.heap().uint8_array_data(value1_obj)?, b"hi");
      } else if scope.heap().is_array_buffer_object(value1_obj) {
        assert_eq!(scope.heap().array_buffer_data(value1_obj)?, b"hi");
      } else {
        return Err(VmError::InvariantViolation(
          "read() result.value must be a Uint8Array or ArrayBuffer",
        ));
      }
    }

    let p2 = realm.exec_script("reader.read()")?;
    let Value::Object(p2_obj) = p2 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(realm.heap().promise_state(p2_obj)?, PromiseState::Fulfilled);
    let Some(result2_val) = realm.heap().promise_result(p2_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Value::Object(result2_obj) = result2_val else {
      return Err(VmError::InvariantViolation("read() must resolve to an object"));
    };

    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done2 = read_result_prop(&mut scope, result2_obj, "done")?;
      assert_eq!(done2, Value::Bool(true));
      let value2 = read_result_prop(&mut scope, result2_obj, "value")?;
      assert!(matches!(value2, Value::Undefined));
    }

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
  fn blob_slice_length_is_zero() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let v = realm.exec_script("Blob.prototype.slice.length")?;
    assert_eq!(v, Value::Number(0.0));

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

  #[test]
  fn blob_string_part_replaces_invalid_surrogates() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let promise = realm.exec_script(r"new Blob(['\uD800']).text()")?;
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
    assert_eq!(get_string(realm.heap(), result), "\u{FFFD}");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn blob_ctor_accepts_file_parts() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let v = realm.exec_script("new Blob([new File(['hi'], 'x.txt')]).size")?;
    assert_eq!(v, Value::Number(2.0));

    let ty = realm.exec_script("(() => { const file = new File(['hi'], 'x.txt', { type: 'text/plain' }); return new Blob([file]).type; })()")?;
    assert_eq!(get_string(realm.heap(), ty), "");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn blob_ctor_treats_detached_buffers_as_empty() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let result = realm.exec_script(
      r#"
(() => {
  // Detach the buffer via the structured clone transfer list.
  if (typeof structuredClone !== "function") {
    return "skip";
  }

  const buf = new ArrayBuffer(4);
  const view = new Uint8Array(buf);
  structuredClone(buf, { transfer: [buf] }); // Detaches `buf`.
  if (buf.byteLength !== 0) {
    return "transfer did not detach";
  }

  try {
    const b = new Blob([buf]);
    if (b.size !== 0) {
      return "ArrayBuffer size was " + b.size;
    }
  } catch (e) {
    return "ArrayBuffer threw " + (e && e.name ? e.name : String(e));
  }

  try {
    const b = new Blob([view]);
    if (b.size !== 0) {
      return "Uint8Array size was " + b.size;
    }
  } catch (e) {
    return "Uint8Array threw " + (e && e.name ? e.name : String(e));
  }

  return "ok";
})()
"#,
    )?;

    let Value::String(s) = result else {
      return Err(VmError::InvariantViolation(
        "expected string result from detached ArrayBuffer empty Blob test",
      ));
    };

    let value = realm.heap().get_string(s)?.to_utf8_lossy();
    if value != "skip" {
      assert_ne!(value, "transfer did not detach");
      assert_eq!(value, "ok");
    }

    realm.teardown();
    Ok(())
  }

  #[test]
  fn blob_ctor_does_not_crash_on_detached_array_buffer() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let result = realm.exec_script(
      r#"
(() => {
  // Detach the buffer via the structured clone transfer list.
  if (typeof structuredClone !== "function") {
    return "skip";
  }

  const buf = new ArrayBuffer(1);
  structuredClone(buf, { transfer: [buf] }); // Detaches `buf`.
  if (buf.byteLength !== 0) {
    return "transfer did not detach";
  }

  const results = [];
  const run = (name, f) => {
    try {
      f();
      results.push(name + ":ok");
    } catch (e) {
      results.push(name + ":" + (e && e.name ? e.name : String(e)));
    }
  };

  // Exercise a few high-level host bindings that call Heap::array_buffer_data.
  run("Blob", () => new Blob([buf]));
  run("TextDecoder", () => new TextDecoder().decode(buf));
  run("Request", () => new Request("https://example.com", { method: "POST", body: buf }));
  run("CryptoDigest", () => crypto.subtle.digest("SHA-256", buf));

  return results.join(",");
})()
"#,
    )?;

    match result {
      Value::String(s) => {
        let name = realm.heap().get_string(s)?.to_utf8_lossy();
        if name != "skip" {
          assert_ne!(name, "transfer did not detach");
          assert!(name.contains("Blob:ok"), "expected Blob result, got {name:?}");
          assert!(
            name.contains("TextDecoder:"),
            "expected TextDecoder result, got {name:?}"
          );
          assert!(name.contains("Request:"), "expected Request result, got {name:?}");
          assert!(
            name.contains("CryptoDigest:"),
            "expected CryptoDigest result, got {name:?}"
          );
          // Allow any JS-catchable error; the invariant is that detached buffers must not abort the
          // VM with a non-catchable `InvariantViolation`.
          assert!(!name.is_empty());
        }
      }
      _other => {
        return Err(VmError::InvariantViolation(
          "expected boolean or string result from detached ArrayBuffer Blob test",
        ));
      }
    }

    realm.teardown();
    Ok(())
  }

  #[test]
  fn blob_type_too_long_is_clamped_to_empty() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let long_len = MAX_BLOB_TYPE_BYTES + 10;
    let ty = realm.exec_script(&format!(
      "(() => {{ const t = 'a'.repeat({long_len}); return new Blob(['hi'], {{ type: t }}).type; }})()"
    ))?;
    assert_eq!(get_string(realm.heap(), ty), "");

    let sliced = realm.exec_script(&format!(
      "(() => {{ const t = 'a'.repeat({long_len}); return new Blob(['hi']).slice(0, 1, t).type; }})()"
    ))?;
    assert_eq!(get_string(realm.heap(), sliced), "");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn blob_ctor_honors_new_target_prototype_for_subclassing() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let result = realm.exec_script(
      r#"
 (() => {
   class X extends Blob {}
  const b = new X(["hi"]);
  if (!(b instanceof X)) return "instanceof X failed";
  if (!(b instanceof Blob)) return "instanceof Blob failed";
  if (Object.getPrototypeOf(b) !== X.prototype) return "prototype mismatch";
  if (b.size !== 2) return "size mismatch: " + b.size;
  return "ok";
 })()
 "#,
    )?;
    assert_eq!(get_string(realm.heap(), result), "ok");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn blob_prototype_property_enumerability_matches_engines() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let result = realm.exec_script(
      r#"
(() => {
  const textDesc = Object.getOwnPropertyDescriptor(Blob.prototype, 'text');
  if (!textDesc) return 'missing Blob.prototype.text descriptor';
  if (textDesc.enumerable !== true) return 'Blob.prototype.text should be enumerable';

  const sliceDesc = Object.getOwnPropertyDescriptor(Blob.prototype, 'slice');
  if (!sliceDesc) return 'missing Blob.prototype.slice descriptor';
  if (sliceDesc.enumerable !== true) return 'Blob.prototype.slice should be enumerable';

  const abDesc = Object.getOwnPropertyDescriptor(Blob.prototype, 'arrayBuffer');
  if (!abDesc) return 'missing Blob.prototype.arrayBuffer descriptor';
  if (abDesc.enumerable !== true) return 'Blob.prototype.arrayBuffer should be enumerable';

  const streamDesc = Object.getOwnPropertyDescriptor(Blob.prototype, 'stream');
  if (!streamDesc) return 'missing Blob.prototype.stream descriptor';
  if (streamDesc.enumerable !== true) return 'Blob.prototype.stream should be enumerable';

  const sizeDesc = Object.getOwnPropertyDescriptor(Blob.prototype, 'size');
  if (!sizeDesc) return 'missing Blob.prototype.size descriptor';
  if (sizeDesc.enumerable !== true) return 'Blob.prototype.size should be enumerable';

  const typeDesc = Object.getOwnPropertyDescriptor(Blob.prototype, 'type');
  if (!typeDesc) return 'missing Blob.prototype.type descriptor';
  if (typeDesc.enumerable !== true) return 'Blob.prototype.type should be enumerable';

  const tagDesc = Object.getOwnPropertyDescriptor(Blob.prototype, Symbol.toStringTag);
  if (!tagDesc) return 'missing Blob.prototype[@@toStringTag] descriptor';
  if (tagDesc.enumerable !== false) return 'Blob.prototype[@@toStringTag] should be non-enumerable';

  return 'ok';
})()
"#,
    )?;

    assert_eq!(get_string(realm.heap(), result), "ok");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn blob_ctor_is_gc_safe_for_getter_returned_parts() -> Result<(), VmError> {
    // Force a GC before (almost) every allocation so we catch missing roots in native code paths.
    let heap_limits = HeapLimits::new(8 * 1024 * 1024, 4 * 1024);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/").with_heap_limits(heap_limits),
    )?;

    let v = realm.exec_script(
      r#"
(() => {
  const parts = [];
  Object.defineProperty(parts, 0, {
    get() { return new Uint8Array([65]); },
    configurable: true,
  });
  parts.length = 1;
  return new Blob(parts, { type: 'text/plain' }).size;
})()
"#,
    )?;
    assert_eq!(v, Value::Number(1.0));

    realm.teardown();
    Ok(())
  }
}
