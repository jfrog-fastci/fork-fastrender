//! Minimal `File` implementation for `vm-js` Window realms.
//!
//! This is a spec-shaped MVP intended to unblock real-world scripts that expect `File` to exist and
//! to allow `fetch()` / `FormData` integrations to use `File` objects.
//!
//! This implementation stores bytes + MIME type in the existing `window_blob` registry so `Blob`
//! methods (`text()`, `arrayBuffer()`, `slice()`) work on `File` objects, and stores `File`-specific
//! metadata (`name`, `lastModified`) in a per-realm registry.
//!
//! Supported FileBits entries (mirrors `Blob`):
//! - `string` (UTF-8 encoded, replacing invalid surrogates)
//! - `ArrayBuffer`
//! - `Uint8Array`
//! - `Blob`/`File`

use std::char::decode_utf16;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use vm_js::{
  GcObject, GcString, Heap, NativeConstructId, NativeFunctionId, PropertyDescriptor, PropertyKey,
  PropertyKind, Realm, RealmId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};

use crate::js::time;
use crate::js::window_blob::{self, BlobData, MAX_BLOB_BYTES};

const FILE_CTOR_REALM_ID_SLOT: usize = 0;
const MAX_FILE_NAME_BYTES: usize = 4 * 1024;
const FILE_NAME_TOO_LONG_ERROR: &str = "File name exceeds maximum length";

#[derive(Clone, Debug)]
pub(crate) struct FileMeta {
  pub(crate) name: String,
  pub(crate) last_modified: i64,
}

#[derive(Default)]
struct FileRegistry {
  realms: HashMap<RealmId, FileRealmState>,
}

struct FileRealmState {
  file_proto: GcObject,
  files: HashMap<WeakGcObject, FileMeta>,
  last_gc_runs: u64,
}

static REGISTRY: OnceLock<Mutex<FileRegistry>> = OnceLock::new();

fn registry() -> &'static Mutex<FileRegistry> {
  REGISTRY.get_or_init(|| Mutex::new(FileRegistry::default()))
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

fn realm_id_for_binding_call(vm: &Vm, scope: &Scope<'_>, callee: GcObject) -> Result<RealmId, VmError> {
  if let Some(realm_id) = vm.current_realm() {
    return Ok(realm_id);
  }

  let slots = scope.heap().get_function_native_slots(callee)?;
  let realm_id = slots
    .get(FILE_CTOR_REALM_ID_SLOT)
    .copied()
    .and_then(realm_id_from_slot)
    .ok_or(VmError::InvariantViolation(
      "File bindings invoked without an active realm",
    ))?;
  Ok(realm_id)
}

fn with_realm_state_mut<R>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  f: impl FnOnce(&mut FileRealmState) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let realm_id = realm_id_for_binding_call(vm, scope, callee)?;

  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  let state = registry
    .realms
    .get_mut(&realm_id)
    .ok_or(VmError::InvariantViolation(
      "File bindings used before install_window_file_bindings",
    ))?;

  // Opportunistically sweep dead Files when GC has run.
  let gc_runs = scope.heap().gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    let heap = scope.heap();
    state.files.retain(|k, _| k.upgrade(heap).is_some());
  }

  f(state)
}

fn js_string_to_utf8_bytes_limited(heap: &Heap, s: GcString, out: &mut Vec<u8>) -> Result<(), VmError> {
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
      return Err(VmError::TypeError("File size exceeds maximum length"));
    }
    out.extend_from_slice(encoded.as_bytes());
  }
  Ok(())
}

fn js_string_to_rust_string_limited(
  heap: &Heap,
  handle: GcString,
  max_bytes: usize,
  error: &'static str,
) -> Result<String, VmError> {
  let js = heap.get_string(handle)?;

  let code_units_len = js.len_code_units();
  // UTF-8 output bytes are always >= UTF-16 code unit length (and can grow by up to 3 bytes per
  // code unit when decoding lone surrogates as U+FFFD). Reject overly large strings up-front to
  // prevent unbounded host allocations.
  if code_units_len > max_bytes {
    return Err(VmError::TypeError(error));
  }

  // Decode manually so we can enforce the byte limit without relying on the potentially-large
  // allocation performed by `String::from_utf16_lossy`.
  let capacity = code_units_len.saturating_mul(3).min(max_bytes);
  let mut out = String::with_capacity(capacity);
  let mut out_len = 0usize;

  for decoded in decode_utf16(js.as_code_units().iter().copied()) {
    let ch = decoded.unwrap_or('\u{FFFD}');
    let ch_len = ch.len_utf8();
    let next_len = out_len.checked_add(ch_len).unwrap_or(usize::MAX);
    if next_len > max_bytes {
      return Err(VmError::TypeError(error));
    }
    out.push(ch);
    out_len = next_len;
  }

  Ok(out)
}

fn append_bytes_limited(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), VmError> {
  let next_len = out.len().checked_add(bytes.len()).ok_or(VmError::OutOfMemory)?;
  if next_len > MAX_BLOB_BYTES {
    return Err(VmError::TypeError("File size exceeds maximum length"));
  }
  out.try_reserve_exact(bytes.len()).map_err(|_| VmError::OutOfMemory)?;
  out.extend_from_slice(bytes);
  Ok(())
}

fn js_value_to_string_limited(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  max_bytes: usize,
  err: &'static str,
) -> Result<String, VmError> {
  let s = scope.to_string(vm, host, hooks, value)?;
  let out = scope.heap().get_string(s)?.to_utf8_lossy();
  if out.len() > max_bytes {
    return Err(VmError::TypeError(err));
  }
  Ok(out)
}

fn to_long_long(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<i64, VmError> {
  if matches!(value, Value::Undefined) {
    return Ok(0);
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

fn file_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("File constructor requires 'new'"))
}

fn file_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "File requires intrinsics (create a Realm first)",
  ))?;

  // Collect the parts up front without holding any registry lock: property access and ToString can
  // invoke user code.
  let bits_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut parts: Vec<Value> = Vec::new();

  if !matches!(bits_val, Value::Undefined | Value::Null) {
    if let Value::Object(bits_obj) = bits_val {
      if scope.heap().object_prototype(bits_obj)? == Some(intr.array_prototype()) {
        let length_key = alloc_key(scope, "length")?;
        let len_val = vm.get_with_host_and_hooks(host, scope, hooks, bits_obj, length_key)?;
        let Value::Number(n) = len_val else {
          return Err(VmError::TypeError("File bits array has invalid length"));
        };
        if !n.is_finite() || n < 0.0 || n > (u32::MAX as f64) {
          return Err(VmError::TypeError("File bits array has invalid length"));
        }
        let len = n as usize;
        parts.try_reserve_exact(len).map_err(|_| VmError::OutOfMemory)?;
        for i in 0..len {
          let key = alloc_key(scope, &i.to_string())?;
          let v = vm.get_with_host_and_hooks(host, scope, hooks, bits_obj, key)?;
          parts.push(v);
        }
      } else {
        parts.push(bits_val);
      }
    } else {
      parts.push(bits_val);
    }
  }

  // Required name argument.
  let name_val = args
    .get(1)
    .copied()
    .ok_or(VmError::TypeError("File constructor requires a name"))?;
  let name_js = scope.to_string(vm, host, hooks, name_val)?;
  let name = js_string_to_rust_string_limited(
    scope.heap(),
    name_js,
    MAX_FILE_NAME_BYTES,
    FILE_NAME_TOO_LONG_ERROR,
  )?;

  // Parse options: { type, lastModified }.
  let mut type_string = String::new();
  let mut last_modified: Option<i64> = None;
  let options_val = args.get(2).copied().unwrap_or(Value::Undefined);
  if !matches!(options_val, Value::Undefined | Value::Null) {
    let Value::Object(options_obj) = options_val else {
      return Err(VmError::TypeError("File options must be an object"));
    };

    let type_key = alloc_key(scope, "type")?;
    let type_val = vm.get_with_host_and_hooks(host, scope, hooks, options_obj, type_key)?;
    if !matches!(type_val, Value::Undefined) {
      let s = scope.to_string(vm, host, hooks, type_val)?;
      type_string = scope.heap().get_string(s)?.to_utf8_lossy();
    }

    let last_modified_key = alloc_key(scope, "lastModified")?;
    let last_modified_val =
      vm.get_with_host_and_hooks(host, scope, hooks, options_obj, last_modified_key)?;
    if !matches!(last_modified_val, Value::Undefined) {
      last_modified = Some(to_long_long(vm, scope, host, hooks, last_modified_val)?);
    }
  }

  let type_string = window_blob::normalize_type(&type_string);
  let last_modified = match last_modified {
    Some(v) => v,
    None => time::date_now_ms(scope)?,
  };

  let mut bytes: Vec<u8> = Vec::new();
  for part in parts {
    if let Value::Object(obj) = part {
      if let Some(blob) = window_blob::clone_blob_data_for_fetch(vm, scope.heap(), Value::Object(obj))? {
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

  let obj = create_file_with_proto(
    vm,
    &mut scope,
    callee,
    proto,
    BlobData {
      bytes,
      r#type: type_string,
    },
    FileMeta {
      name,
      last_modified,
    },
  )?;

  Ok(Value::Object(obj))
}

pub(crate) fn clone_file_metadata_for_fetch(
  vm: &Vm,
  heap: &Heap,
  value: Value,
) -> Result<Option<FileMeta>, VmError> {
  let Value::Object(obj) = value else {
    return Ok(None);
  };
  let Some(realm_id) = vm.current_realm() else {
    return Ok(None);
  };

  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  let Some(state) = registry.realms.get_mut(&realm_id) else {
    return Ok(None);
  };

  // Opportunistic sweep.
  let gc_runs = heap.gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    state.files.retain(|k, _| k.upgrade(heap).is_some());
  }

  Ok(state.files.get(&WeakGcObject::from(obj)).cloned())
}

pub(crate) fn file_prototype_for_realm(realm_id: RealmId) -> Option<GcObject> {
  let registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  registry.realms.get(&realm_id).map(|s| s.file_proto)
}

pub(crate) fn create_file_with_proto(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  proto: GcObject,
  data: BlobData,
  meta: FileMeta,
) -> Result<GcObject, VmError> {
  let obj = window_blob::create_blob_with_proto(vm, scope, callee, proto, data)?;

  // Root `obj` while allocating property keys/strings: these can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;

  let name_key = alloc_key(&mut scope, "name")?;
  let name_js = scope.alloc_string(&meta.name)?;
  scope.push_root(Value::String(name_js))?;
  scope.define_property(obj, name_key, data_desc(Value::String(name_js), false))?;

  let last_modified_key = alloc_key(&mut scope, "lastModified")?;
  scope.define_property(
    obj,
    last_modified_key,
    data_desc(Value::Number(meta.last_modified as f64), false),
  )?;

  let webkit_relative_path_key = alloc_key(&mut scope, "webkitRelativePath")?;
  let empty = scope.alloc_string("")?;
  scope.push_root(Value::String(empty))?;
  scope.define_property(
    obj,
    webkit_relative_path_key,
    data_desc(Value::String(empty), false),
  )?;

  with_realm_state_mut(vm, &mut scope, callee, |state| {
    state.files.insert(WeakGcObject::from(obj), meta);
    Ok(())
  })?;

  Ok(obj)
}

pub fn install_window_file_bindings(vm: &mut Vm, realm: &Realm, heap: &mut Heap) -> Result<(), VmError> {
  let intr = realm.intrinsics();
  let realm_id = realm.id();

  let blob_proto = window_blob::blob_prototype_for_realm(realm_id).ok_or(VmError::Unimplemented(
    "File requires Blob to be installed",
  ))?;

  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let call_id: NativeFunctionId = vm.register_native_call(file_ctor_call)?;
  let construct_id: NativeConstructId = vm.register_native_construct(file_ctor_construct)?;

  let name = scope.alloc_string("File")?;
  scope.push_root(Value::String(name))?;
  let ctor = scope.alloc_native_function_with_slots(
    call_id,
    Some(construct_id),
    name,
    2,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(ctor))?;
  let file_ctor_proto = {
    let blob_ctor_key = alloc_key(&mut scope, "Blob")?;
    scope
      .heap()
      .object_get_own_data_property_value(global, &blob_ctor_key)?
      .unwrap_or(Value::Undefined)
  };
  scope.heap_mut().object_set_prototype(
    ctor,
    match file_ctor_proto {
      Value::Object(obj) => Some(obj),
      _ => Some(intr.function_prototype()),
    },
  )?;

  let proto = {
    let key = alloc_key(&mut scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(ctor, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => return Err(VmError::InvariantViolation("File constructor missing prototype object")),
    }
  };
  scope.push_root(Value::Object(proto))?;
  scope.heap_mut().object_set_prototype(proto, Some(blob_proto))?;

  let to_string_tag = intr.well_known_symbols().to_string_tag;
  let tag_key = PropertyKey::from_symbol(to_string_tag);
  let tag_value = scope.alloc_string("File")?;
  scope.push_root(Value::String(tag_value))?;
  scope.define_property(proto, tag_key, data_desc(Value::String(tag_value), false))?;

  let ctor_key = alloc_key(&mut scope, "File")?;
  scope.define_property(global, ctor_key, data_desc(Value::Object(ctor), true))?;

  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  registry.realms.insert(
    realm_id,
    FileRealmState {
      file_proto: proto,
      files: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
    },
  );

  Ok(())
}

pub fn teardown_window_file_bindings_for_realm(realm_id: RealmId) {
  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  registry.realms.remove(&realm_id);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::clock::VirtualClock;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use crate::js::WebTime;
  use std::sync::Arc;
  use std::time::Duration;

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value, got {value:?}");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn file_constructor_and_properties() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let v = realm.exec_script("typeof File")?;
    assert_eq!(get_string(realm.heap(), v), "function");

    let v = realm.exec_script(
      "new File(['hi'], 'a.txt', { type: 'Text/Plain', lastModified: 123 }).size",
    )?;
    assert_eq!(v, Value::Number(2.0));

    let v = realm.exec_script(
      "new File(['hi'], 'a.txt', { type: 'Text/Plain', lastModified: 123 }).type",
    )?;
    assert_eq!(get_string(realm.heap(), v), "text/plain");

    let v = realm.exec_script("new File(['hi'], 'a.txt', { lastModified: 123 }).name")?;
    assert_eq!(get_string(realm.heap(), v), "a.txt");

    let v = realm.exec_script("new File(['hi'], 'a.txt', { lastModified: 123 }).lastModified")?;
    assert_eq!(v, Value::Number(123.0));

    let v = realm.exec_script("Object.prototype.toString.call(new File(['hi'], 'a.txt'))")?;
    assert_eq!(get_string(realm.heap(), v), "[object File]");

    let v = realm.exec_script(
      "(() => { const f = new File(['hi'], 'a.txt'); return (f instanceof File) && (f instanceof Blob); })()",
    )?;
    assert_eq!(v, Value::Bool(true));

    let v = realm.exec_script(
      "Object.prototype.toString.call(new File(['hi'], 'a.txt').slice(0, 1))",
    )?;
    assert_eq!(get_string(realm.heap(), v), "[object Blob]");

    let v = realm.exec_script("new File(['hi'], 'a.txt').slice(0, 1) instanceof File")?;
    assert_eq!(v, Value::Bool(false));

    let v = realm.exec_script("new File(['x'], 'x.txt').webkitRelativePath")?;
    assert_eq!(get_string(realm.heap(), v), "");

    let v = realm.exec_script("Object.getPrototypeOf(File) === Blob")?;
    assert_eq!(v, Value::Bool(true));

    let v = realm.exec_script(
      "(() => {\
        try {\
          new File(['x'], 'a'.repeat(10_000));\
          return false;\
        } catch (e) {\
          return (e instanceof TypeError) && String(e).includes('maximum length');\
        }\
      })()",
    )?;
    assert_eq!(v, Value::Bool(true));

    realm.teardown();
    Ok(())
  }

  #[test]
  fn file_default_last_modified_is_deterministic_and_ignores_date_now_override() -> Result<(), VmError> {
    let clock = Arc::new(VirtualClock::new());
    clock.set_now(Duration::from_millis(1234));
    let config = WindowRealmConfig::new("https://example.com/")
      .with_clock(clock)
      .with_web_time(WebTime::new(1000));
    let mut realm = WindowRealm::new(config)?;

    let v = realm.exec_script(
      "(() => {\
        Date.now = () => 5;\
        return new File(['hi'], 'x.txt').lastModified;\
      })()",
    )?;
    assert_eq!(v, Value::Number(2234.0));

    realm.teardown();
    Ok(())
  }
}
