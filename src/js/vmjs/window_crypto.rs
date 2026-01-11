//! Minimal `window.crypto` bindings for `vm-js` Window realms.
//!
//! This module implements the RNG-facing subset of WebCrypto that is commonly used by real-world
//! scripts:
//! - `crypto.getRandomValues(Uint8Array)`
//! - `crypto.randomUUID()`
//!
//! ## Determinism
//!
//! FastRender's JS embedding intentionally avoids OS randomness so renderer outputs and unit tests
//! are stable. We implement a per-realm deterministic PRNG using **xorshift64\***, seeded from a
//! stable hash of `WindowRealmConfig.document_url`.
//!
//! The PRNG state is stored in [`crate::js::window_realm::WindowRealmUserData`] so each realm has an
//! isolated RNG stream.

use crate::js::window_realm::WindowRealmUserData;
use vm_js::{
  GcObject, Heap, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError,
  VmHost, VmHostHooks,
};

const MAX_GET_RANDOM_VALUES_BYTES: usize = 65_536;

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

fn read_only_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
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

fn set_own_data_prop(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
  writable: bool,
) -> Result<(), VmError> {
  // Root `obj` + `value` while allocating the property key: string allocation can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(value, writable))
}

fn create_dom_exception_like(scope: &mut Scope<'_>, name: &str, message: &str) -> Result<Value, VmError> {
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;

  let name_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(name_s))?;
  let message_s = scope.alloc_string(message)?;
  scope.push_root(Value::String(message_s))?;

  let name_key = alloc_key(scope, "name")?;
  let message_key = alloc_key(scope, "message")?;

  scope.define_property(obj, name_key, read_only_data_desc(Value::String(name_s)))?;
  scope.define_property(
    obj,
    message_key,
    read_only_data_desc(Value::String(message_s)),
  )?;

  Ok(Value::Object(obj))
}

fn hash_u64(input: &str) -> u64 {
  // 64-bit FNV-1a.
  let mut hash: u64 = 0xcbf29ce484222325;
  for &b in input.as_bytes() {
    hash ^= u64::from(b);
    hash = hash.wrapping_mul(0x100000001b3);
  }
  hash
}

pub(crate) fn crypto_rng_seed_from_document_url(document_url: &str) -> u64 {
  // Deterministic seed derived from document URL so different pages diverge while staying
  // reproducible across runs.
  //
  // xorshift64* requires a non-zero state; ensure we never seed with 0.
  let mut seed = hash_u64(document_url) ^ 0x9E3779B97F4A7C15;
  if seed == 0 {
    seed = 0x2545F4914F6CDD1D;
  }
  seed
}

fn xorshift64star_next(state: &mut u64) -> u64 {
  // xorshift64*
  let mut x = *state;
  debug_assert_ne!(x, 0, "xorshift64* state must be non-zero");
  x ^= x >> 12;
  x ^= x << 25;
  x ^= x >> 27;
  *state = x;
  x.wrapping_mul(0x2545F4914F6CDD1D)
}

fn crypto_rng_fill_bytes(vm: &mut Vm, out: &mut [u8]) -> Result<(), VmError> {
  let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
    return Err(VmError::InvariantViolation("window realm missing user data"));
  };

  for chunk in out.chunks_mut(8) {
    let rand = xorshift64star_next(&mut data.crypto_rng_state);
    let bytes = rand.to_le_bytes();
    chunk.copy_from_slice(&bytes[..chunk.len()]);
  }
  Ok(())
}

fn crypto_ctor_illegal_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("Illegal constructor"))
}

fn crypto_ctor_illegal_construct(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  Err(VmError::TypeError("Illegal constructor"))
}

fn crypto_get_random_values_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(array_obj) = arg else {
    return Err(VmError::TypeError("crypto.getRandomValues expects a Uint8Array"));
  };

  if !scope.heap().is_uint8_array_object(array_obj) {
    return Err(VmError::TypeError("crypto.getRandomValues expects a Uint8Array"));
  }

  let len = {
    // Avoid holding the heap borrow across the subsequent write loop.
    scope.heap().uint8_array_data(array_obj)?.len()
  };

  if len > MAX_GET_RANDOM_VALUES_BYTES {
    // WebCrypto quota: 65536 bytes.
    return Err(VmError::Throw(create_dom_exception_like(
      scope,
      "QuotaExceededError",
      "",
    )?));
  }

  let mut offset = 0usize;
  let mut buf = [0u8; 64];
  while offset < len {
    let n = (len - offset).min(buf.len());
    crypto_rng_fill_bytes(vm, &mut buf[..n])?;
    let wrote = scope.heap_mut().uint8_array_write(array_obj, offset, &buf[..n])?;
    debug_assert_eq!(wrote, n, "uint8_array_write should write full chunk");
    offset += wrote;
  }

  Ok(Value::Object(array_obj))
}

fn crypto_random_uuid_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let mut bytes = [0u8; 16];
  crypto_rng_fill_bytes(vm, &mut bytes)?;

  // RFC 4122 v4: set version (4) and variant (10).
  bytes[6] = (bytes[6] & 0x0F) | 0x40;
  bytes[8] = (bytes[8] & 0x3F) | 0x80;

  const HEX: &[u8; 16] = b"0123456789abcdef";
  let mut out = String::with_capacity(36);
  for (i, &b) in bytes.iter().enumerate() {
    if matches!(i, 4 | 6 | 8 | 10) {
      out.push('-');
    }
    out.push(HEX[(b >> 4) as usize] as char);
    out.push(HEX[(b & 0x0F) as usize] as char);
  }

  Ok(Value::String(scope.alloc_string(&out)?))
}

fn subtle_unimplemented_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("Unimplemented"))
}

pub(crate) fn install_window_crypto_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
) -> Result<(), VmError> {
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let intr = realm.intrinsics();
  let func_proto = intr.function_prototype();

  // --- Crypto prototype + methods -------------------------------------------------------------
  let crypto_proto = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(crypto_proto))?;

  // Object.prototype.toString branding ([object Crypto]) via Symbol.toStringTag.
  let to_string_tag_key = PropertyKey::from_symbol(realm.well_known_symbols().to_string_tag);
  let crypto_tag = scope.alloc_string("Crypto")?;
  scope.push_root(Value::String(crypto_tag))?;
  scope.define_property(
    crypto_proto,
    to_string_tag_key,
    read_only_data_desc(Value::String(crypto_tag)),
  )?;

  // crypto.getRandomValues(typedArray)
  let get_random_values_id = vm.register_native_call(crypto_get_random_values_native)?;
  let get_random_values_name = scope.alloc_string("getRandomValues")?;
  scope.push_root(Value::String(get_random_values_name))?;
  let get_random_values_fn =
    scope.alloc_native_function(get_random_values_id, None, get_random_values_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(get_random_values_fn, Some(func_proto))?;
  set_own_data_prop(
    &mut scope,
    crypto_proto,
    "getRandomValues",
    Value::Object(get_random_values_fn),
    /* writable */ true,
  )?;

  // crypto.randomUUID()
  let random_uuid_id = vm.register_native_call(crypto_random_uuid_native)?;
  let random_uuid_name = scope.alloc_string("randomUUID")?;
  scope.push_root(Value::String(random_uuid_name))?;
  let random_uuid_fn = scope.alloc_native_function(random_uuid_id, None, random_uuid_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(random_uuid_fn, Some(func_proto))?;
  set_own_data_prop(
    &mut scope,
    crypto_proto,
    "randomUUID",
    Value::Object(random_uuid_fn),
    /* writable */ true,
  )?;

  // --- Crypto constructor (illegal) -----------------------------------------------------------
  let crypto_call_id = vm.register_native_call(crypto_ctor_illegal_call)?;
  let crypto_construct_id = vm.register_native_construct(crypto_ctor_illegal_construct)?;
  let crypto_ctor_name = scope.alloc_string("Crypto")?;
  scope.push_root(Value::String(crypto_ctor_name))?;
  let crypto_ctor = scope.alloc_native_function(
    crypto_call_id,
    Some(crypto_construct_id),
    crypto_ctor_name,
    0,
  )?;
  scope
    .heap_mut()
    .object_set_prototype(crypto_ctor, Some(func_proto))?;
  scope.push_root(Value::Object(crypto_ctor))?;

  let prototype_key = alloc_key(&mut scope, "prototype")?;
  let constructor_key = alloc_key(&mut scope, "constructor")?;
  scope.define_property(
    crypto_ctor,
    prototype_key,
    ctor_link_desc(Value::Object(crypto_proto)),
  )?;
  scope.define_property(
    crypto_proto,
    constructor_key,
    ctor_link_desc(Value::Object(crypto_ctor)),
  )?;

  // --- crypto instance -----------------------------------------------------------------------
  let crypto_obj = scope.alloc_object_with_prototype(Some(crypto_proto))?;
  scope.push_root(Value::Object(crypto_obj))?;

  // Optional stub: crypto.subtle
  let subtle_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(subtle_obj))?;

  let subtle_unimpl_id = vm.register_native_call(subtle_unimplemented_native)?;
  for (name, arity) in [
    ("encrypt", 3),
    ("decrypt", 3),
    ("sign", 3),
    ("verify", 4),
    ("digest", 2),
    ("generateKey", 3),
    ("deriveKey", 5),
    ("deriveBits", 3),
    ("importKey", 5),
    ("exportKey", 2),
    ("wrapKey", 4),
    ("unwrapKey", 5),
  ] {
    let name_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(name_s))?;
    let func = scope.alloc_native_function(subtle_unimpl_id, None, name_s, arity)?;
    scope.heap_mut().object_set_prototype(func, Some(func_proto))?;
    set_own_data_prop(&mut scope, subtle_obj, name, Value::Object(func), /* writable */ true)?;
  }

  set_own_data_prop(
    &mut scope,
    crypto_obj,
    "subtle",
    Value::Object(subtle_obj),
    /* writable */ false,
  )?;

  // Expose on global.
  let crypto_key = alloc_key(&mut scope, "crypto")?;
  scope.define_property(global, crypto_key, read_only_data_desc(Value::Object(crypto_obj)))?;

  let crypto_ctor_key = alloc_key(&mut scope, "Crypto")?;
  scope.define_property(global, crypto_ctor_key, data_desc(Value::Object(crypto_ctor), true))?;

  Ok(())
}
