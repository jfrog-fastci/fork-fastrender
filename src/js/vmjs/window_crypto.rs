//! Minimal `window.crypto` bindings for `vm-js` Window realms.
//!
//! This module implements the RNG-facing subset of WebCrypto that is commonly used by real-world
//! scripts:
//! - `crypto.getRandomValues(typedArray)`
//! - `crypto.randomUUID()`
//!
//! ## Determinism
//!
//! FastRender's JS embedding intentionally avoids OS randomness so renderer outputs and unit tests
//! are stable. We implement a per-realm deterministic PRNG using **xorshift64\***, seeded from a
//! stable hash of `WindowRealmConfig.document_url` by default.
//!
//! Embeddings/tests can override the seed via `WindowRealmConfig.crypto_rng_seed` to force a
//! specific stream of random values.
//!
//! The PRNG state is stored in `WindowRealmUserData` so each realm has an
//! isolated RNG stream.

use crate::js::window_realm::WindowRealmUserData;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Aes256Gcm};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use vm_js::{
  new_promise_capability_with_host_and_hooks, new_range_error, new_type_error_object, GcObject,
  Heap, HostSlots, Intrinsics, PromiseCapability, PropertyDescriptor, PropertyKey, PropertyKind,
  Realm, RealmId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};

const MAX_GET_RANDOM_VALUES_BYTES: usize = 65_536;
const MAX_DIGEST_INPUT_BYTES: usize = 10 * 1024 * 1024;

// HostSlots tags for platform objects installed by this module.
//
// These are only used for branding: structuredClone must reject them as platform objects.
const CRYPTO_HOST_TAG: u64 = 0x4352_5950_544F_5F5F; // "CRYPTO__"
const SUBTLE_CRYPTO_HOST_TAG: u64 = 0x5355_4254_4C45_5F5F; // "SUBTLE__"
const CRYPTO_KEY_HOST_TAG: u64 = 0x4352_5950_544B_4559; // "CRYPTOKEY"

// `aes-gcm` ships type aliases for AES-128/256-GCM, but not AES-192-GCM.
type Aes192Gcm = aes_gcm::AesGcm<aes::Aes192, aes_gcm::aead::consts::U12>;

#[derive(Clone, Debug)]
enum CryptoKeyAlgorithm {
  AesGcm { length_bytes: usize },
  HmacSha256,
}

#[derive(Clone, Debug)]
struct CryptoKeyState {
  algorithm: CryptoKeyAlgorithm,
  key_bytes: Vec<u8>,
  extractable: bool,
  usages: Vec<String>,
}

#[derive(Default)]
struct CryptoRegistry {
  realms: HashMap<RealmId, CryptoRealmState>,
}

struct CryptoRealmState {
  last_gc_runs: u64,
  keys: HashMap<WeakGcObject, CryptoKeyState>,
}

static CRYPTO_REGISTRY: OnceLock<Mutex<CryptoRegistry>> = OnceLock::new();

fn crypto_registry() -> &'static Mutex<CryptoRegistry> {
  CRYPTO_REGISTRY.get_or_init(|| Mutex::new(CryptoRegistry::default()))
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

fn realm_id_for_crypto_call(vm: &Vm, scope: &Scope<'_>, callee: GcObject) -> Result<RealmId, VmError> {
  if let Some(realm_id) = vm.current_realm() {
    return Ok(realm_id);
  }

  let slots = scope.heap().get_function_native_slots(callee)?;
  slots
    .get(0)
    .copied()
    .and_then(realm_id_from_slot)
    .ok_or(VmError::InvariantViolation(
      "crypto.subtle binding invoked without an active realm",
    ))
}

fn sweep_crypto_realm_state_if_needed(state: &mut CryptoRealmState, heap: &Heap) {
  let gc_runs = heap.gc_runs();
  if gc_runs == state.last_gc_runs {
    return;
  }
  state.keys.retain(|k, _| k.upgrade(heap).is_some());
  state.last_gc_runs = gc_runs;
}

fn data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn enumerable_data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
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

fn set_own_enumerable_data_prop(
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
  scope.define_property(obj, key, enumerable_data_desc(value, writable))
}

fn get_prop(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  name: &str,
) -> Result<Value, VmError> {
  // Root `obj` while allocating the key and performing the get: either can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  let key = alloc_key(&mut scope, name)?;
  vm.get_with_host_and_hooks(&mut *host, &mut scope, &mut *hooks, obj, key)
}

fn create_dom_exception_like(
  scope: &mut Scope<'_>,
  name: &str,
  message: &str,
) -> Result<Value, VmError> {
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

fn resolve_promise_with_value(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  cap: &PromiseCapability,
  value: Value,
) -> Result<(), VmError> {
  vm.call_with_host_and_hooks(
    &mut *host,
    scope,
    &mut *hooks,
    cap.resolve,
    Value::Undefined,
    &[value],
  )?;
  Ok(())
}

fn reject_promise_with_value(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  cap: &PromiseCapability,
  value: Value,
) -> Result<(), VmError> {
  vm.call_with_host_and_hooks(
    &mut *host,
    scope,
    &mut *hooks,
    cap.reject,
    Value::Undefined,
    &[value],
  )?;
  Ok(())
}

fn reject_promise_with_vm_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  cap: &PromiseCapability,
  intr: &Intrinsics,
  err: VmError,
) -> Result<(), VmError> {
  if let Some(thrown) = err.thrown_value() {
    return reject_promise_with_value(vm, scope, host, hooks, cap, thrown);
  }
  match err {
    VmError::TypeError(msg) => {
      let err_value = new_type_error_object(scope, intr, msg)?;
      reject_promise_with_value(vm, scope, host, hooks, cap, err_value)
    }
    VmError::RangeError(msg) => {
      // Minimal compatibility: surface RangeErrors from conversions as TypeError instances.
      let err_value = new_type_error_object(scope, intr, msg)?;
      reject_promise_with_value(vm, scope, host, hooks, cap, err_value)
    }
    other if other.is_throw_completion() => {
      let err_value = new_type_error_object(scope, intr, &other.to_string())?;
      reject_promise_with_value(vm, scope, host, hooks, cap, err_value)
    }
    other => Err(other),
  }
}

fn buffer_source_to_bytes(scope: &Scope<'_>, value: Value) -> Result<Vec<u8>, VmError> {
  let Value::Object(obj) = value else {
    return Err(VmError::TypeError("Expected BufferSource"));
  };
  let heap = scope.heap();
  if heap.is_array_buffer_object(obj) {
    return Ok(heap.array_buffer_data(obj)?.to_vec());
  }
  if heap.is_typed_array_object(obj) {
    let (buffer_obj, byte_offset, byte_len) = heap.typed_array_view_bytes(obj)?;
    let buf_bytes = heap.array_buffer_data(buffer_obj)?;
    let end = byte_offset
      .checked_add(byte_len)
      .ok_or(VmError::InvariantViolation("TypedArray byte offset overflow"))?;
    return Ok(
      buf_bytes
        .get(byte_offset..end)
        .ok_or(VmError::InvariantViolation("TypedArray view out of bounds"))?
        .to_vec(),
    );
  }
  if heap.is_data_view_object(obj) {
    let buffer_obj = heap.data_view_buffer(obj)?;
    let byte_offset = heap.data_view_byte_offset(obj)?;
    let byte_len = heap.data_view_byte_length(obj)?;
    let buf_bytes = heap.array_buffer_data(buffer_obj)?;
    let end = byte_offset
      .checked_add(byte_len)
      .ok_or(VmError::InvariantViolation("DataView byte offset overflow"))?;
    return Ok(
      buf_bytes
        .get(byte_offset..end)
        .ok_or(VmError::InvariantViolation("DataView view out of bounds"))?
        .to_vec(),
    );
  }
  Err(VmError::TypeError("Expected BufferSource"))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
  // RFC 2104 (HMAC) with SHA-256 (64-byte block).
  const BLOCK_SIZE: usize = 64;
  let mut key_block = [0u8; BLOCK_SIZE];

  if key.len() > BLOCK_SIZE {
    let digest = Sha256::digest(key);
    key_block[..digest.len()].copy_from_slice(&digest);
  } else {
    key_block[..key.len()].copy_from_slice(key);
  }

  let mut o_key_pad = [0u8; BLOCK_SIZE];
  let mut i_key_pad = [0u8; BLOCK_SIZE];
  for i in 0..BLOCK_SIZE {
    o_key_pad[i] = key_block[i] ^ 0x5c;
    i_key_pad[i] = key_block[i] ^ 0x36;
  }

  let mut inner = Sha256::new();
  inner.update(i_key_pad);
  inner.update(data);
  let inner_digest = inner.finalize();

  let mut outer = Sha256::new();
  outer.update(o_key_pad);
  outer.update(inner_digest);
  let out = outer.finalize();

  let mut sig = [0u8; 32];
  sig.copy_from_slice(&out);
  sig
}

fn require_crypto_key(scope: &Scope<'_>, value: Value) -> Result<GcObject, VmError> {
  let Value::Object(obj) = value else {
    return Err(VmError::TypeError("Expected CryptoKey"));
  };
  let Some(slots) = scope.heap().object_host_slots(obj)? else {
    return Err(VmError::TypeError("Expected CryptoKey"));
  };
  if slots.a != CRYPTO_KEY_HOST_TAG {
    return Err(VmError::TypeError("Expected CryptoKey"));
  }
  Ok(obj)
}

fn crypto_realm_state_mut<'a>(
  realm_id: RealmId,
  heap: &Heap,
  registry: &'a mut CryptoRegistry,
) -> &'a mut CryptoRealmState {
  let state = registry.realms.entry(realm_id).or_insert_with(|| CryptoRealmState {
    last_gc_runs: 0,
    keys: HashMap::new(),
  });
  sweep_crypto_realm_state_if_needed(state, heap);
  state
}

fn crypto_key_state_lookup(
  realm_id: RealmId,
  heap: &Heap,
  key_obj: GcObject,
) -> Result<CryptoKeyState, VmError> {
  let mut registry = crypto_registry().lock().unwrap_or_else(|err| err.into_inner());
  let state = crypto_realm_state_mut(realm_id, heap, &mut registry);
  state
    .keys
    .get(&WeakGcObject::new(key_obj))
    .cloned()
    .ok_or(VmError::TypeError("Unknown CryptoKey"))
}

fn crypto_key_state_insert(realm_id: RealmId, heap: &Heap, key_obj: GcObject, state: CryptoKeyState) {
  let mut registry = crypto_registry().lock().unwrap_or_else(|err| err.into_inner());
  let realm_state = crypto_realm_state_mut(realm_id, heap, &mut registry);
  realm_state.keys.insert(WeakGcObject::new(key_obj), state);
}

fn alloc_array_with_prototype(
  intr: &Intrinsics,
  scope: &mut Scope<'_>,
  len: usize,
) -> Result<GcObject, VmError> {
  let arr = scope.alloc_array(len)?;
  scope.push_root(Value::Object(arr))?;
  scope
    .heap_mut()
    .object_set_prototype(arr, Some(intr.array_prototype()))?;
  Ok(arr)
}

fn fill_string_array(
  scope: &mut Scope<'_>,
  arr: GcObject,
  values: &[String],
) -> Result<(), VmError> {
  // Root `arr` while allocating strings for indices.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(arr))?;
  for (i, v) in values.iter().enumerate() {
    let s = scope.alloc_string(v)?;
    scope.push_root(Value::String(s))?;
    let key = alloc_key(&mut scope, &i.to_string())?;
    scope.define_property(
      arr,
      key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::String(s),
          writable: true,
        },
      },
    )?;
  }
  Ok(())
}

fn array_length(scope: &mut Scope<'_>, arr: GcObject) -> Result<usize, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(arr))?;
  let len_key = alloc_key(&mut scope, "length")?;
  let len = scope
    .heap()
    .object_get_own_data_property_value(arr, &len_key)?
    .unwrap_or(Value::Undefined);
  match len {
    Value::Number(n) if n.is_finite() && n >= 0.0 => Ok(n.trunc() as usize),
    _ => Ok(0),
  }
}

fn parse_string_array(scope: &mut Scope<'_>, value: Value, err: &'static str) -> Result<Vec<String>, VmError> {
  let Value::Object(arr) = value else {
    return Err(VmError::TypeError(err));
  };
  if !scope.heap().object_is_array(arr)? {
    return Err(VmError::TypeError(err));
  }
  let len = array_length(scope, arr)?;
  let mut out = Vec::with_capacity(len);
  // Root the array while allocating index keys + strings.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(arr))?;
  for i in 0..len {
    let key = alloc_key(&mut scope, &i.to_string())?;
    let v = scope
      .heap()
      .object_get_own_data_property_value(arr, &key)?
      .unwrap_or(Value::Undefined);
    let Value::String(s) = v else {
      return Err(VmError::TypeError(err));
    };
    out.push(scope.heap().get_string(s)?.to_utf8_lossy());
  }
  Ok(out)
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

pub(crate) fn crypto_rng_seed_from_u64(seed: u64) -> u64 {
  // xorshift64* requires a non-zero state; normalize 0 to a fixed non-zero constant so
  // callers/tests can pass `0` without crashing the PRNG.
  if seed == 0 {
    0x2545F4914F6CDD1D
  } else {
    seed
  }
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
    return Err(VmError::InvariantViolation(
      "window realm missing user data",
    ));
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
  const TYPE_ERROR_MSG: &str = "crypto.getRandomValues expects an integer TypedArray";

  let arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(array_obj) = arg else {
    return Err(VmError::TypeError(TYPE_ERROR_MSG));
  };

  // Root the argument object for the duration of any property gets / allocations (property access
  // can run JS and trigger GC).
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(array_obj))?;

  // Reject non-typed arrays (plain objects, ArrayBuffer, DataView, etc). The WebCrypto spec only
  // accepts integer TypedArray variants.
  if !scope.heap().is_typed_array_object(array_obj) {
    return Err(VmError::TypeError(TYPE_ERROR_MSG));
  }

  // WebCrypto spec: fill the **bytes** of the passed TypedArray view, respecting `byteOffset` and
  // `byteLength`, and return the same object.
  //
  // We implement this by writing through a `Uint8Array` view:
  // - If the input is a native `Uint8Array`, write directly to it.
  // - Otherwise, create a temporary `Uint8Array` view over the same bytes for efficient filling.

  // Spec: only integer typed arrays are accepted.
  if !scope.heap().typed_array_is_integer_kind(array_obj)? {
    return Err(VmError::TypeError(TYPE_ERROR_MSG));
  }

  let (buffer_obj, byte_offset, byte_len) = scope.heap().typed_array_view_bytes(array_obj)?;

  // WebCrypto quota: 65536 bytes per call (measured on the view's byte length).
  if byte_len > MAX_GET_RANDOM_VALUES_BYTES {
    return Err(VmError::Throw(create_dom_exception_like(
      &mut scope,
      "QuotaExceededError",
      "",
    )?));
  }

  let write_view_obj = if scope.heap().is_uint8_array_object(array_obj) {
    array_obj
  } else {
    // Allocate a temporary Uint8Array view over the same bytes so we can populate it efficiently
    // via `Heap::uint8_array_write`.
    scope.alloc_uint8_array(buffer_obj, byte_offset, byte_len)?
  };

  let mut offset = 0usize;
  let mut buf = [0u8; 64];
  while offset < byte_len {
    let n = (byte_len - offset).min(buf.len());
    crypto_rng_fill_bytes(vm, &mut buf[..n])?;
    let wrote = scope
      .heap_mut()
      .uint8_array_write(write_view_obj, offset, &buf[..n])?;
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
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // WebCrypto spec shape: SubtleCrypto methods always return a Promise (reject immediately for
  // unimplemented methods in this MVP implementation).
  let cap: PromiseCapability =
    new_promise_capability_with_host_and_hooks(vm, scope, &mut *host, &mut *hooks)?;

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  // Root the Promise capability components while allocating the error object.
  let mut scope = scope.reborrow();
  scope.push_root(cap.promise)?;
  scope.push_root(cap.resolve)?;
  scope.push_root(cap.reject)?;

  let err = new_type_error_object(&mut scope, &intr, "Unimplemented")?;
  let err = scope.push_root(err)?;
  vm.call_with_host_and_hooks(
    &mut *host,
    &mut scope,
    &mut *hooks,
    cap.reject,
    Value::Undefined,
    &[err],
  )?;

  Ok(cap.promise)
}

fn subtle_digest_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // WebCrypto spec shape: always return a Promise (resolve/reject immediately for this MVP
  // implementation).
  let cap: PromiseCapability =
    new_promise_capability_with_host_and_hooks(vm, scope, &mut *host, &mut *hooks)?;

  // `NewPromiseCapability` requires intrinsics, so this should always be available if we reached
  // this point.
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  // Root the Promise capability components while we allocate strings / ArrayBuffers below.
  let mut scope = scope.reborrow();
  scope.push_root(cap.promise)?;
  scope.push_root(cap.resolve)?;
  scope.push_root(cap.reject)?;

  let algorithm = args.get(0).copied().unwrap_or(Value::Undefined);
  let data = args.get(1).copied().unwrap_or(Value::Undefined);

  #[derive(Clone, Copy)]
  enum DigestAlg {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
  }

  let reject_with_value = |vm: &mut Vm,
                           scope: &mut Scope<'_>,
                           host: &mut dyn VmHost,
                           hooks: &mut dyn VmHostHooks,
                           cap: PromiseCapability,
                           err_value: Value|
   -> Result<Value, VmError> {
    // Root `err_value`: `reject` can run user JS and trigger GC.
    let err_value = scope.push_root(err_value)?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      &mut *hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
    Ok(cap.promise)
  };

  let reject_with_vm_error = |vm: &mut Vm,
                              scope: &mut Scope<'_>,
                              host: &mut dyn VmHost,
                              hooks: &mut dyn VmHostHooks,
                              cap: PromiseCapability,
                              err: VmError|
   -> Result<Value, VmError> {
    if let Some(thrown) = err.thrown_value() {
      return reject_with_value(vm, scope, host, hooks, cap, thrown);
    }
    match err {
      VmError::TypeError(msg) => {
        let err_value = new_type_error_object(scope, &intr, msg)?;
        reject_with_value(vm, scope, host, hooks, cap, err_value)
      }
      VmError::RangeError(msg) => {
        let err_value = new_range_error(scope, intr, msg)?;
        reject_with_value(vm, scope, host, hooks, cap, err_value)
      }
      other if other.is_throw_completion() => {
        let err_value = new_type_error_object(scope, &intr, &other.to_string())?;
        reject_with_value(vm, scope, host, hooks, cap, err_value)
      }
      other => Err(other),
    }
  };

  // --- Normalize AlgorithmIdentifier ----------------------------------------------------------
  // Spec-ish behavior:
  // - If passed a string, use it.
  // - If passed an object, read `algorithm.name` using ordinary property get and coerce to string.
  let algorithm_name: String = match algorithm {
    Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
    Value::Object(obj) => {
      scope.push_root(Value::Object(obj))?;
      let name_key = alloc_key(&mut scope, "name")?;
      let name_value = match vm.get_with_host_and_hooks(&mut *host, &mut scope, &mut *hooks, obj, name_key) {
        Ok(v) => v,
        Err(err) => return reject_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, cap, err),
      };
      if matches!(name_value, Value::Undefined) {
        let err = new_type_error_object(
          &mut scope,
          &intr,
          "crypto.subtle.digest expects an algorithm name string",
        )?;
        return reject_with_value(vm, &mut scope, &mut *host, &mut *hooks, cap, err);
      }
      // Root `name_value`: `ToString` can invoke user code and trigger GC.
      let name_value = scope.push_root(name_value)?;
      let name_string = match scope.to_string(vm, &mut *host, &mut *hooks, name_value) {
        Ok(s) => s,
        Err(err) => return reject_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, cap, err),
      };
      scope.heap().get_string(name_string)?.to_utf8_lossy()
    }
    _ => {
      let err = new_type_error_object(
        &mut scope,
        &intr,
        "crypto.subtle.digest expects an algorithm name string",
      )?;
      return reject_with_value(vm, &mut scope, &mut *host, &mut *hooks, cap, err);
    }
  };

  let digest_alg: Option<DigestAlg> = {
    let name = algorithm_name.trim();
    if name.eq_ignore_ascii_case("SHA-1") || name.eq_ignore_ascii_case("SHA1") {
      Some(DigestAlg::Sha1)
    } else if name.eq_ignore_ascii_case("SHA-256") || name.eq_ignore_ascii_case("SHA256") {
      Some(DigestAlg::Sha256)
    } else if name.eq_ignore_ascii_case("SHA-384") || name.eq_ignore_ascii_case("SHA384") {
      Some(DigestAlg::Sha384)
    } else if name.eq_ignore_ascii_case("SHA-512") || name.eq_ignore_ascii_case("SHA512") {
      Some(DigestAlg::Sha512)
    } else {
      None
    }
  };

  let Some(digest_alg) = digest_alg else {
    let message = format!("Unsupported digest algorithm: {algorithm_name}");
    let err = create_dom_exception_like(&mut scope, "NotSupportedError", &message)?;
    return reject_with_value(vm, &mut scope, &mut *host, &mut *hooks, cap, err);
  };

  let compute_digest = |bytes: &[u8]| -> Vec<u8> {
    match digest_alg {
      DigestAlg::Sha1 => Sha1::digest(bytes).to_vec(),
      DigestAlg::Sha256 => Sha256::digest(bytes).to_vec(),
      DigestAlg::Sha384 => Sha384::digest(bytes).to_vec(),
      DigestAlg::Sha512 => Sha512::digest(bytes).to_vec(),
    }
  };

  // --- Read BufferSource ----------------------------------------------------------------------
  //
  // We hash directly from the underlying ArrayBuffer backing store (respecting view byteOffset /
  // byteLength) so we don't need to allocate/copy the entire input into a temporary Vec.
  let digest = match data {
    Value::Object(obj) => {
      scope.push_root(Value::Object(obj))?;
      let heap = scope.heap();
      if heap.is_array_buffer_object(obj) {
        let bytes = match heap.array_buffer_data(obj) {
          Ok(bytes) => bytes,
          Err(err) => {
            return reject_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, cap, err)
          }
        };
        if bytes.len() > MAX_DIGEST_INPUT_BYTES {
          return reject_with_vm_error(
            vm,
            &mut scope,
            &mut *host,
            &mut *hooks,
            cap,
            VmError::RangeError("crypto.subtle.digest input too large"),
          );
        }
        compute_digest(bytes)
      } else if heap.is_typed_array_object(obj) {
        let (buffer_obj, byte_offset, byte_len) = match heap.typed_array_view_bytes(obj) {
          Ok(v) => v,
          Err(err) => {
            return reject_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, cap, err)
          }
        };
        if byte_len > MAX_DIGEST_INPUT_BYTES {
          return reject_with_vm_error(
            vm,
            &mut scope,
            &mut *host,
            &mut *hooks,
            cap,
            VmError::RangeError("crypto.subtle.digest input too large"),
          );
        }
        let buf_bytes = match heap.array_buffer_data(buffer_obj) {
          Ok(b) => b,
          Err(err) => {
            return reject_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, cap, err)
          }
        };
        let end = byte_offset
          .checked_add(byte_len)
          .ok_or(VmError::InvariantViolation("TypedArray byte offset overflow"))?;
        let view_bytes = buf_bytes
          .get(byte_offset..end)
          .ok_or(VmError::InvariantViolation("TypedArray view out of bounds"))?;
        compute_digest(view_bytes)
      } else if heap.is_data_view_object(obj) {
        let buffer_obj = heap.data_view_buffer(obj)?;
        let byte_offset = heap.data_view_byte_offset(obj)?;
        let byte_len = heap.data_view_byte_length(obj)?;
        if byte_len > MAX_DIGEST_INPUT_BYTES {
          return reject_with_vm_error(
            vm,
            &mut scope,
            &mut *host,
            &mut *hooks,
            cap,
            VmError::RangeError("crypto.subtle.digest input too large"),
          );
        }
        let buf_bytes = match heap.array_buffer_data(buffer_obj) {
          Ok(b) => b,
          Err(err) => {
            return reject_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, cap, err)
          }
        };
        let end = byte_offset
          .checked_add(byte_len)
          .ok_or(VmError::InvariantViolation("DataView byte offset overflow"))?;
        let view_bytes = buf_bytes
          .get(byte_offset..end)
          .ok_or(VmError::InvariantViolation("DataView view out of bounds"))?;
        compute_digest(view_bytes)
      } else {
        let err =
          new_type_error_object(&mut scope, &intr, "crypto.subtle.digest expects a BufferSource")?;
        return reject_with_value(vm, &mut scope, &mut *host, &mut *hooks, cap, err);
      }
    }
    _ => {
      let err =
        new_type_error_object(&mut scope, &intr, "crypto.subtle.digest expects a BufferSource")?;
      return reject_with_value(vm, &mut scope, &mut *host, &mut *hooks, cap, err);
    }
  };

  let ab = scope.alloc_array_buffer_from_u8_vec(digest)?;
  scope
    .heap_mut()
    .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
  let ab_val = scope.push_root(Value::Object(ab))?;
  vm.call_with_host_and_hooks(
    &mut *host,
    &mut scope,
    &mut *hooks,
    cap.resolve,
    Value::Undefined,
    &[ab_val],
  )?;

  Ok(cap.promise)
}

fn subtle_import_key_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let cap: PromiseCapability =
    new_promise_capability_with_host_and_hooks(vm, scope, &mut *host, &mut *hooks)?;
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  let mut scope = scope.reborrow();
  scope.push_root(cap.promise)?;
  scope.push_root(cap.resolve)?;
  scope.push_root(cap.reject)?;

  let format = args.get(0).copied().unwrap_or(Value::Undefined);
  let key_data = args.get(1).copied().unwrap_or(Value::Undefined);
  let algorithm = args.get(2).copied().unwrap_or(Value::Undefined);
  let extractable_v = args.get(3).copied().unwrap_or(Value::Undefined);
  let key_usages_v = args.get(4).copied().unwrap_or(Value::Undefined);

  let format_name = match format {
    Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
    _ => {
      let err = new_type_error_object(&mut scope, &intr, "crypto.subtle.importKey expects a format string")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    }
  };

  if !format_name.eq_ignore_ascii_case("jwk") {
    let err = create_dom_exception_like(&mut scope, "NotSupportedError", "Unsupported key format")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  // --- Parse key usages (sequence<DOMString>) --------------------------------------------------
  let requested_usages = match parse_string_array(&mut scope, key_usages_v, "crypto.subtle.importKey expects keyUsages to be an Array of strings") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };

  let extractable = match scope.heap().to_boolean(extractable_v) {
    Ok(b) => b,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };

  // --- Normalize algorithm --------------------------------------------------------------------
  enum ImportAlg {
    AesGcm,
    HmacSha256,
  }

  let (alg_name, alg_obj) = match algorithm {
    Value::String(s) => (scope.heap().get_string(s)?.to_utf8_lossy(), None),
    Value::Object(obj) => {
      scope.push_root(Value::Object(obj))?;
      let name_value = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, obj, "name") {
        Ok(v) => v,
        Err(err) => {
          reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
          return Ok(cap.promise);
        }
      };
      if matches!(name_value, Value::Undefined) {
        let err = new_type_error_object(&mut scope, &intr, "crypto.subtle.importKey expects an algorithm name")?;
        reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
        return Ok(cap.promise);
      }
      let name_string = match scope.to_string(vm, &mut *host, &mut *hooks, name_value) {
        Ok(s) => s,
        Err(err) => {
          reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
          return Ok(cap.promise);
        }
      };
      (scope.heap().get_string(name_string)?.to_utf8_lossy(), Some(obj))
    }
    _ => {
      let err = new_type_error_object(&mut scope, &intr, "crypto.subtle.importKey expects an algorithm identifier")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    }
  };

  let alg = if alg_name.trim().eq_ignore_ascii_case("AES-GCM") {
    ImportAlg::AesGcm
  } else if alg_name.trim().eq_ignore_ascii_case("HMAC") {
    // WebCrypto requires `hash` for HMAC import.
    let Some(alg_obj) = alg_obj else {
      let err = new_type_error_object(&mut scope, &intr, "crypto.subtle.importKey for HMAC expects an algorithm object with a hash")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    };
    let hash_value = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, alg_obj, "hash") {
      Ok(v) => v,
      Err(err) => {
        reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
        return Ok(cap.promise);
      }
    };
    let hash_name: String = match hash_value {
      Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
      Value::Object(hash_obj) => {
        scope.push_root(Value::Object(hash_obj))?;
        let name_value = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, hash_obj, "name") {
          Ok(v) => v,
          Err(err) => {
            reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
            return Ok(cap.promise);
          }
        };
        let name_string = match scope.to_string(vm, &mut *host, &mut *hooks, name_value) {
          Ok(s) => s,
          Err(err) => {
            reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
            return Ok(cap.promise);
          }
        };
        scope.heap().get_string(name_string)?.to_utf8_lossy()
      }
      _ => {
        let err = new_type_error_object(&mut scope, &intr, "crypto.subtle.importKey for HMAC expects a hash algorithm")?;
        reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
        return Ok(cap.promise);
      }
    };
    if !hash_name.trim().eq_ignore_ascii_case("SHA-256") && !hash_name.trim().eq_ignore_ascii_case("SHA256") {
      let err = create_dom_exception_like(&mut scope, "NotSupportedError", "Unsupported HMAC hash")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    }
    ImportAlg::HmacSha256
  } else {
    let err = create_dom_exception_like(&mut scope, "NotSupportedError", "Unsupported algorithm")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  };

  // --- Parse JWK ------------------------------------------------------------------------------
  let Value::Object(jwk_obj) = key_data else {
    let err = new_type_error_object(&mut scope, &intr, "crypto.subtle.importKey expects keyData to be an object")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  };
  scope.push_root(Value::Object(jwk_obj))?;

  let kty = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, jwk_obj, "kty") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let Value::String(kty_s) = kty else {
    let err = create_dom_exception_like(&mut scope, "DataError", "JWK kty must be a string")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  };
  let kty_str = scope.heap().get_string(kty_s)?.to_utf8_lossy();
  if kty_str != "oct" {
    let err = create_dom_exception_like(&mut scope, "DataError", "Unsupported JWK kty")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  let k_val = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, jwk_obj, "k") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let Value::String(k_s) = k_val else {
    let err = create_dom_exception_like(&mut scope, "DataError", "JWK k must be a string")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  };
  let k_str = scope.heap().get_string(k_s)?.to_utf8_lossy();
  let key_bytes = match URL_SAFE_NO_PAD.decode(k_str.as_bytes()) {
    Ok(b) => b,
    Err(_) => {
      let err = create_dom_exception_like(&mut scope, "DataError", "Invalid JWK base64url encoding")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    }
  };

  // ext rules: reject if ext:false but extractable:true.
  let ext_val = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, jwk_obj, "ext") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  if !matches!(ext_val, Value::Undefined) {
    let Value::Bool(ext_b) = ext_val else {
      let err = create_dom_exception_like(&mut scope, "DataError", "JWK ext must be a boolean")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    };
    if !ext_b && extractable {
      let err = create_dom_exception_like(&mut scope, "DataError", "Key is not extractable")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    }
  }

  let jwk_alg_val = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, jwk_obj, "alg") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let jwk_alg: Option<String> = if matches!(jwk_alg_val, Value::Undefined) {
    None
  } else {
    let Value::String(s) = jwk_alg_val else {
      let err = create_dom_exception_like(&mut scope, "DataError", "JWK alg must be a string")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    };
    Some(scope.heap().get_string(s)?.to_utf8_lossy())
  };

  // key_ops rules: if present, it must contain all requested usages.
  let key_ops_val = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, jwk_obj, "key_ops") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  if !matches!(key_ops_val, Value::Undefined) {
    let key_ops = match parse_string_array(&mut scope, key_ops_val, "JWK key_ops must be an array of strings") {
      Ok(v) => v,
      Err(_) => {
        let err = create_dom_exception_like(&mut scope, "DataError", "JWK key_ops must be an array of strings")?;
        reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
        return Ok(cap.promise);
      }
    };
    let ops: HashSet<&str> = key_ops.iter().map(|s| s.as_str()).collect();
    if requested_usages.iter().any(|u| !ops.contains(u.as_str())) {
      let err = create_dom_exception_like(&mut scope, "DataError", "JWK key_ops does not allow requested usages")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    }
  }

  // Validate key length and JWK alg.
  let key_state_alg: CryptoKeyAlgorithm = match alg {
    ImportAlg::AesGcm => {
      let len = key_bytes.len();
      if !matches!(len, 16 | 24 | 32) {
        let err = create_dom_exception_like(&mut scope, "DataError", "Invalid AES-GCM key length")?;
        reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
        return Ok(cap.promise);
      }
      if let Some(jwk_alg) = jwk_alg.as_deref() {
        let expected = match len {
          16 => "A128GCM",
          24 => "A192GCM",
          32 => "A256GCM",
          _ => unreachable!(), // fastrender-allow-panic
        };
        if jwk_alg != expected {
          let err = create_dom_exception_like(&mut scope, "DataError", "JWK alg does not match AES-GCM key length")?;
          reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
          return Ok(cap.promise);
        }
      }
      CryptoKeyAlgorithm::AesGcm { length_bytes: len }
    }
    ImportAlg::HmacSha256 => {
      if key_bytes.is_empty() {
        let err = create_dom_exception_like(&mut scope, "DataError", "Invalid HMAC key length")?;
        reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
        return Ok(cap.promise);
      }
      if let Some(jwk_alg) = jwk_alg.as_deref() {
        if jwk_alg != "HS256" {
          let err = create_dom_exception_like(&mut scope, "DataError", "JWK alg does not match HMAC-SHA256")?;
          reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
          return Ok(cap.promise);
        }
      }
      CryptoKeyAlgorithm::HmacSha256
    }
  };

  // Ensure usages are compatible with the algorithm.
  let allowed_usages: HashSet<&'static str> = match key_state_alg {
    CryptoKeyAlgorithm::AesGcm { .. } => ["encrypt", "decrypt", "wrapKey", "unwrapKey"].into_iter().collect(),
    CryptoKeyAlgorithm::HmacSha256 => ["sign", "verify"].into_iter().collect(),
  };
  if requested_usages.iter().any(|u| !allowed_usages.contains(u.as_str())) {
    let err = create_dom_exception_like(&mut scope, "DataError", "Invalid key usages for algorithm")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  // --- Create CryptoKey object ----------------------------------------------------------------
  let key_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(key_obj))?;
  scope.heap_mut().object_set_host_slots(
    key_obj,
    HostSlots {
      a: CRYPTO_KEY_HOST_TAG,
      b: 0,
    },
  )?;

  // `type` = "secret"
  let type_s = scope.alloc_string("secret")?;
  scope.push_root(Value::String(type_s))?;
  set_own_data_prop(&mut scope, key_obj, "type", Value::String(type_s), false)?;
  set_own_data_prop(
    &mut scope,
    key_obj,
    "extractable",
    Value::Bool(extractable),
    false,
  )?;

  // algorithm object
  let alg_obj_js = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(alg_obj_js))?;
  match key_state_alg {
    CryptoKeyAlgorithm::AesGcm { length_bytes } => {
      let name_s = scope.alloc_string("AES-GCM")?;
      scope.push_root(Value::String(name_s))?;
      set_own_data_prop(&mut scope, alg_obj_js, "name", Value::String(name_s), false)?;
      set_own_data_prop(
        &mut scope,
        alg_obj_js,
        "length",
        Value::Number((length_bytes * 8) as f64),
        false,
      )?;
    }
    CryptoKeyAlgorithm::HmacSha256 => {
      let name_s = scope.alloc_string("HMAC")?;
      scope.push_root(Value::String(name_s))?;
      set_own_data_prop(&mut scope, alg_obj_js, "name", Value::String(name_s), false)?;

      let hash_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
      scope.push_root(Value::Object(hash_obj))?;
      let hash_name_s = scope.alloc_string("SHA-256")?;
      scope.push_root(Value::String(hash_name_s))?;
      set_own_data_prop(&mut scope, hash_obj, "name", Value::String(hash_name_s), false)?;
      set_own_data_prop(&mut scope, alg_obj_js, "hash", Value::Object(hash_obj), false)?;

      set_own_data_prop(
        &mut scope,
        alg_obj_js,
        "length",
        Value::Number((key_bytes.len() * 8) as f64),
        false,
      )?;
    }
  }
  set_own_data_prop(&mut scope, key_obj, "algorithm", Value::Object(alg_obj_js), false)?;

  // usages array
  let usages_arr = alloc_array_with_prototype(&intr, &mut scope, requested_usages.len())?;
  fill_string_array(&mut scope, usages_arr, &requested_usages)?;
  set_own_data_prop(&mut scope, key_obj, "usages", Value::Object(usages_arr), false)?;

  let realm_id = realm_id_for_crypto_call(vm, &scope, callee)?;
  crypto_key_state_insert(
    realm_id,
    scope.heap(),
    key_obj,
    CryptoKeyState {
      algorithm: key_state_alg,
      key_bytes,
      extractable,
      usages: requested_usages,
    },
  );

  resolve_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, Value::Object(key_obj))?;
  Ok(cap.promise)
}

fn subtle_export_key_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let cap: PromiseCapability =
    new_promise_capability_with_host_and_hooks(vm, scope, &mut *host, &mut *hooks)?;
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  let mut scope = scope.reborrow();
  scope.push_root(cap.promise)?;
  scope.push_root(cap.resolve)?;
  scope.push_root(cap.reject)?;

  let format = args.get(0).copied().unwrap_or(Value::Undefined);
  let key_value = args.get(1).copied().unwrap_or(Value::Undefined);

  let format_name = match format {
    Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
    _ => {
      let err = new_type_error_object(&mut scope, &intr, "crypto.subtle.exportKey expects a format string")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    }
  };
  if !format_name.eq_ignore_ascii_case("jwk") {
    let err = create_dom_exception_like(&mut scope, "NotSupportedError", "Unsupported export format")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  let key_obj = match require_crypto_key(&scope, key_value) {
    Ok(obj) => obj,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  scope.push_root(Value::Object(key_obj))?;

  let realm_id = realm_id_for_crypto_call(vm, &scope, callee)?;
  let key_state = match crypto_key_state_lookup(realm_id, scope.heap(), key_obj) {
    Ok(s) => s,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };

  if !key_state.extractable {
    let err = create_dom_exception_like(&mut scope, "InvalidAccessError", "Key is not extractable")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  let jwk_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(jwk_obj))?;

  let kty_s = scope.alloc_string("oct")?;
  scope.push_root(Value::String(kty_s))?;
  set_own_enumerable_data_prop(&mut scope, jwk_obj, "kty", Value::String(kty_s), true)?;

  let k_encoded = URL_SAFE_NO_PAD.encode(&key_state.key_bytes);
  let k_s = scope.alloc_string(&k_encoded)?;
  scope.push_root(Value::String(k_s))?;
  set_own_enumerable_data_prop(&mut scope, jwk_obj, "k", Value::String(k_s), true)?;

  set_own_enumerable_data_prop(
    &mut scope,
    jwk_obj,
    "ext",
    Value::Bool(key_state.extractable),
    true,
  )?;

  let ops_arr = alloc_array_with_prototype(&intr, &mut scope, key_state.usages.len())?;
  fill_string_array(&mut scope, ops_arr, &key_state.usages)?;
  set_own_enumerable_data_prop(&mut scope, jwk_obj, "key_ops", Value::Object(ops_arr), true)?;

  // Set `alg` when unambiguous.
  let alg_str: Option<&'static str> = match key_state.algorithm {
    CryptoKeyAlgorithm::HmacSha256 => Some("HS256"),
    CryptoKeyAlgorithm::AesGcm { length_bytes } => match length_bytes {
      16 => Some("A128GCM"),
      24 => Some("A192GCM"),
      32 => Some("A256GCM"),
      _ => None,
    },
  };
  if let Some(alg_str) = alg_str {
    let alg_s = scope.alloc_string(alg_str)?;
    scope.push_root(Value::String(alg_s))?;
    set_own_enumerable_data_prop(&mut scope, jwk_obj, "alg", Value::String(alg_s), true)?;
  }

  resolve_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, Value::Object(jwk_obj))?;
  Ok(cap.promise)
}

fn subtle_encrypt_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let cap: PromiseCapability =
    new_promise_capability_with_host_and_hooks(vm, scope, &mut *host, &mut *hooks)?;
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  let mut scope = scope.reborrow();
  scope.push_root(cap.promise)?;
  scope.push_root(cap.resolve)?;
  scope.push_root(cap.reject)?;

  let algorithm = args.get(0).copied().unwrap_or(Value::Undefined);
  let key_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let data_value = args.get(2).copied().unwrap_or(Value::Undefined);

  let alg_obj = match algorithm {
    Value::Object(obj) => obj,
    _ => {
      let err = new_type_error_object(&mut scope, &intr, "crypto.subtle.encrypt expects an algorithm object")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    }
  };
  scope.push_root(Value::Object(alg_obj))?;

  let name_value = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, alg_obj, "name") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let name_string = match scope.to_string(vm, &mut *host, &mut *hooks, name_value) {
    Ok(s) => s,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let alg_name = scope.heap().get_string(name_string)?.to_utf8_lossy();
  if !alg_name.trim().eq_ignore_ascii_case("AES-GCM") {
    let err = create_dom_exception_like(&mut scope, "NotSupportedError", "Unsupported algorithm")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  let key_obj = match require_crypto_key(&scope, key_value) {
    Ok(obj) => obj,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  scope.push_root(Value::Object(key_obj))?;

  let realm_id = realm_id_for_crypto_call(vm, &scope, callee)?;
  let key_state = match crypto_key_state_lookup(realm_id, scope.heap(), key_obj) {
    Ok(s) => s,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let CryptoKeyAlgorithm::AesGcm { length_bytes } = key_state.algorithm else {
    let err = create_dom_exception_like(&mut scope, "DataError", "CryptoKey is not an AES-GCM key")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  };
  if !key_state.usages.iter().any(|u| u == "encrypt") {
    let err = create_dom_exception_like(&mut scope, "DataError", "CryptoKey does not allow encrypt")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  let iv_value = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, alg_obj, "iv") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let iv_bytes = match buffer_source_to_bytes(&scope, iv_value) {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  if iv_bytes.len() != 12 {
    let err = create_dom_exception_like(&mut scope, "DataError", "AES-GCM iv must be 12 bytes")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  let additional_data_value = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, alg_obj, "additionalData") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let additional_data = if matches!(additional_data_value, Value::Undefined) {
    Vec::new()
  } else {
    match buffer_source_to_bytes(&scope, additional_data_value) {
      Ok(v) => v,
      Err(err) => {
        reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
        return Ok(cap.promise);
      }
    }
  };

  let tag_len_value = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, alg_obj, "tagLength") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  if !matches!(tag_len_value, Value::Undefined) {
    let Value::Number(n) = tag_len_value else {
      let err = new_type_error_object(&mut scope, &intr, "AES-GCM tagLength must be a number")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    };
    if n != 128.0 {
      let err = create_dom_exception_like(&mut scope, "NotSupportedError", "Unsupported AES-GCM tagLength")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    }
  }

  let plaintext = match buffer_source_to_bytes(&scope, data_value) {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };

  let nonce = aes_gcm::Nonce::from_slice(&iv_bytes);
  let ciphertext = {
    let payload = Payload {
      msg: &plaintext,
      aad: &additional_data,
    };
    let result: Result<Vec<u8>, ()> = match length_bytes {
      16 => match Aes128Gcm::new_from_slice(&key_state.key_bytes) {
        Ok(cipher) => cipher.encrypt(nonce, payload).map_err(|_| ()),
        Err(_) => Err(()),
      },
      24 => match Aes192Gcm::new_from_slice(&key_state.key_bytes) {
        Ok(cipher) => cipher.encrypt(nonce, payload).map_err(|_| ()),
        Err(_) => Err(()),
      },
      32 => match Aes256Gcm::new_from_slice(&key_state.key_bytes) {
        Ok(cipher) => cipher.encrypt(nonce, payload).map_err(|_| ()),
        Err(_) => Err(()),
      },
      _ => Err(()),
    };
    match result {
      Ok(v) => v,
      Err(_) => {
        let err = create_dom_exception_like(&mut scope, "OperationError", "AES-GCM encryption failed")?;
        reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
        return Ok(cap.promise);
      }
    }
  };

  let ab = scope.alloc_array_buffer_from_u8_vec(ciphertext)?;
  scope
    .heap_mut()
    .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
  resolve_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, Value::Object(ab))?;
  Ok(cap.promise)
}

fn subtle_decrypt_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let cap: PromiseCapability =
    new_promise_capability_with_host_and_hooks(vm, scope, &mut *host, &mut *hooks)?;
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  let mut scope = scope.reborrow();
  scope.push_root(cap.promise)?;
  scope.push_root(cap.resolve)?;
  scope.push_root(cap.reject)?;

  let algorithm = args.get(0).copied().unwrap_or(Value::Undefined);
  let key_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let data_value = args.get(2).copied().unwrap_or(Value::Undefined);

  let alg_obj = match algorithm {
    Value::Object(obj) => obj,
    _ => {
      let err = new_type_error_object(&mut scope, &intr, "crypto.subtle.decrypt expects an algorithm object")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    }
  };
  scope.push_root(Value::Object(alg_obj))?;

  let name_value = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, alg_obj, "name") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let name_string = match scope.to_string(vm, &mut *host, &mut *hooks, name_value) {
    Ok(s) => s,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let alg_name = scope.heap().get_string(name_string)?.to_utf8_lossy();
  if !alg_name.trim().eq_ignore_ascii_case("AES-GCM") {
    let err = create_dom_exception_like(&mut scope, "NotSupportedError", "Unsupported algorithm")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  let key_obj = match require_crypto_key(&scope, key_value) {
    Ok(obj) => obj,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  scope.push_root(Value::Object(key_obj))?;

  let realm_id = realm_id_for_crypto_call(vm, &scope, callee)?;
  let key_state = match crypto_key_state_lookup(realm_id, scope.heap(), key_obj) {
    Ok(s) => s,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let CryptoKeyAlgorithm::AesGcm { length_bytes } = key_state.algorithm else {
    let err = create_dom_exception_like(&mut scope, "DataError", "CryptoKey is not an AES-GCM key")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  };
  if !key_state.usages.iter().any(|u| u == "decrypt") {
    let err = create_dom_exception_like(&mut scope, "DataError", "CryptoKey does not allow decrypt")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  let iv_value = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, alg_obj, "iv") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let iv_bytes = match buffer_source_to_bytes(&scope, iv_value) {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  if iv_bytes.len() != 12 {
    let err = create_dom_exception_like(&mut scope, "DataError", "AES-GCM iv must be 12 bytes")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  let additional_data_value = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, alg_obj, "additionalData") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let additional_data = if matches!(additional_data_value, Value::Undefined) {
    Vec::new()
  } else {
    match buffer_source_to_bytes(&scope, additional_data_value) {
      Ok(v) => v,
      Err(err) => {
        reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
        return Ok(cap.promise);
      }
    }
  };

  let tag_len_value = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, alg_obj, "tagLength") {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  if !matches!(tag_len_value, Value::Undefined) {
    let Value::Number(n) = tag_len_value else {
      let err = new_type_error_object(&mut scope, &intr, "AES-GCM tagLength must be a number")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    };
    if n != 128.0 {
      let err = create_dom_exception_like(&mut scope, "NotSupportedError", "Unsupported AES-GCM tagLength")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    }
  }

  let ciphertext = match buffer_source_to_bytes(&scope, data_value) {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };

  let nonce = aes_gcm::Nonce::from_slice(&iv_bytes);
  let plaintext = {
    let payload = Payload {
      msg: &ciphertext,
      aad: &additional_data,
    };
    let result: Result<Vec<u8>, ()> = match length_bytes {
      16 => match Aes128Gcm::new_from_slice(&key_state.key_bytes) {
        Ok(cipher) => cipher.decrypt(nonce, payload).map_err(|_| ()),
        Err(_) => Err(()),
      },
      24 => match Aes192Gcm::new_from_slice(&key_state.key_bytes) {
        Ok(cipher) => cipher.decrypt(nonce, payload).map_err(|_| ()),
        Err(_) => Err(()),
      },
      32 => match Aes256Gcm::new_from_slice(&key_state.key_bytes) {
        Ok(cipher) => cipher.decrypt(nonce, payload).map_err(|_| ()),
        Err(_) => Err(()),
      },
      _ => Err(()),
    };
    match result {
      Ok(v) => v,
      Err(_) => {
        let err = create_dom_exception_like(&mut scope, "OperationError", "AES-GCM decryption failed")?;
        reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
        return Ok(cap.promise);
      }
    }
  };

  let ab = scope.alloc_array_buffer_from_u8_vec(plaintext)?;
  scope
    .heap_mut()
    .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
  resolve_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, Value::Object(ab))?;
  Ok(cap.promise)
}

fn subtle_sign_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let cap: PromiseCapability =
    new_promise_capability_with_host_and_hooks(vm, scope, &mut *host, &mut *hooks)?;
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  let mut scope = scope.reborrow();
  scope.push_root(cap.promise)?;
  scope.push_root(cap.resolve)?;
  scope.push_root(cap.reject)?;

  let algorithm = args.get(0).copied().unwrap_or(Value::Undefined);
  let key_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let data_value = args.get(2).copied().unwrap_or(Value::Undefined);

  let alg_name: String = match algorithm {
    Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
    Value::Object(obj) => {
      scope.push_root(Value::Object(obj))?;
      let name_value = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, obj, "name") {
        Ok(v) => v,
        Err(err) => {
          reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
          return Ok(cap.promise);
        }
      };
      let name_string = match scope.to_string(vm, &mut *host, &mut *hooks, name_value) {
        Ok(s) => s,
        Err(err) => {
          reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
          return Ok(cap.promise);
        }
      };
      scope.heap().get_string(name_string)?.to_utf8_lossy()
    }
    _ => {
      let err = new_type_error_object(&mut scope, &intr, "crypto.subtle.sign expects an algorithm identifier")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    }
  };

  if !alg_name.trim().eq_ignore_ascii_case("HMAC") {
    let err = create_dom_exception_like(&mut scope, "NotSupportedError", "Unsupported algorithm")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  let key_obj = match require_crypto_key(&scope, key_value) {
    Ok(obj) => obj,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  scope.push_root(Value::Object(key_obj))?;

  let realm_id = realm_id_for_crypto_call(vm, &scope, callee)?;
  let key_state = match crypto_key_state_lookup(realm_id, scope.heap(), key_obj) {
    Ok(s) => s,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  if !matches!(key_state.algorithm, CryptoKeyAlgorithm::HmacSha256) {
    let err = create_dom_exception_like(&mut scope, "DataError", "CryptoKey is not an HMAC-SHA256 key")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }
  if !key_state.usages.iter().any(|u| u == "sign") {
    let err = create_dom_exception_like(&mut scope, "DataError", "CryptoKey does not allow sign")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  let data_bytes = match buffer_source_to_bytes(&scope, data_value) {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };

  let sig = hmac_sha256(&key_state.key_bytes, &data_bytes).to_vec();
  let ab = scope.alloc_array_buffer_from_u8_vec(sig)?;
  scope
    .heap_mut()
    .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
  resolve_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, Value::Object(ab))?;
  Ok(cap.promise)
}

fn subtle_verify_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let cap: PromiseCapability =
    new_promise_capability_with_host_and_hooks(vm, scope, &mut *host, &mut *hooks)?;
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  let mut scope = scope.reborrow();
  scope.push_root(cap.promise)?;
  scope.push_root(cap.resolve)?;
  scope.push_root(cap.reject)?;

  let algorithm = args.get(0).copied().unwrap_or(Value::Undefined);
  let key_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let signature_value = args.get(2).copied().unwrap_or(Value::Undefined);
  let data_value = args.get(3).copied().unwrap_or(Value::Undefined);

  let alg_name: String = match algorithm {
    Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
    Value::Object(obj) => {
      scope.push_root(Value::Object(obj))?;
      let name_value = match get_prop(vm, &mut scope, &mut *host, &mut *hooks, obj, "name") {
        Ok(v) => v,
        Err(err) => {
          reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
          return Ok(cap.promise);
        }
      };
      let name_string = match scope.to_string(vm, &mut *host, &mut *hooks, name_value) {
        Ok(s) => s,
        Err(err) => {
          reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
          return Ok(cap.promise);
        }
      };
      scope.heap().get_string(name_string)?.to_utf8_lossy()
    }
    _ => {
      let err = new_type_error_object(&mut scope, &intr, "crypto.subtle.verify expects an algorithm identifier")?;
      reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
      return Ok(cap.promise);
    }
  };

  if !alg_name.trim().eq_ignore_ascii_case("HMAC") {
    let err = create_dom_exception_like(&mut scope, "NotSupportedError", "Unsupported algorithm")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  let key_obj = match require_crypto_key(&scope, key_value) {
    Ok(obj) => obj,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  scope.push_root(Value::Object(key_obj))?;

  let realm_id = realm_id_for_crypto_call(vm, &scope, callee)?;
  let key_state = match crypto_key_state_lookup(realm_id, scope.heap(), key_obj) {
    Ok(s) => s,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  if !matches!(key_state.algorithm, CryptoKeyAlgorithm::HmacSha256) {
    let err = create_dom_exception_like(&mut scope, "DataError", "CryptoKey is not an HMAC-SHA256 key")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }
  if !key_state.usages.iter().any(|u| u == "verify") {
    let err = create_dom_exception_like(&mut scope, "DataError", "CryptoKey does not allow verify")?;
    reject_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, err)?;
    return Ok(cap.promise);
  }

  let sig_bytes = match buffer_source_to_bytes(&scope, signature_value) {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };
  let data_bytes = match buffer_source_to_bytes(&scope, data_value) {
    Ok(v) => v,
    Err(err) => {
      reject_promise_with_vm_error(vm, &mut scope, &mut *host, &mut *hooks, &cap, &intr, err)?;
      return Ok(cap.promise);
    }
  };

  let expected = hmac_sha256(&key_state.key_bytes, &data_bytes);
  let ok = sig_bytes.as_slice() == expected.as_slice();
  resolve_promise_with_value(vm, &mut scope, &mut *host, &mut *hooks, &cap, Value::Bool(ok))?;
  Ok(cap.promise)
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
  let realm_id = realm.id();

  // Initialize per-realm CryptoKey registry.
  {
    let mut registry = crypto_registry().lock().unwrap_or_else(|err| err.into_inner());
    registry
      .realms
      .entry(realm_id)
      .or_insert_with(|| CryptoRealmState {
        last_gc_runs: 0,
        keys: HashMap::new(),
      });
  }

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
  scope.heap_mut().object_set_host_slots(
    crypto_obj,
    HostSlots {
      a: CRYPTO_HOST_TAG,
      b: 0,
    },
  )?;

  // Optional stub: crypto.subtle
  let subtle_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(subtle_obj))?;
  scope.heap_mut().object_set_host_slots(
    subtle_obj,
    HostSlots {
      a: SUBTLE_CRYPTO_HOST_TAG,
      b: 0,
      },
    )?;

  // Object.prototype.toString branding ([object SubtleCrypto]) via Symbol.toStringTag.
  let subtle_tag = scope.alloc_string("SubtleCrypto")?;
  scope.push_root(Value::String(subtle_tag))?;
  scope.define_property(
    subtle_obj,
    to_string_tag_key,
    read_only_data_desc(Value::String(subtle_tag)),
  )?;

  let subtle_unimpl_id = vm.register_native_call(subtle_unimplemented_native)?;
  let subtle_digest_id = vm.register_native_call(subtle_digest_native)?;
  let subtle_encrypt_id = vm.register_native_call(subtle_encrypt_native)?;
  let subtle_decrypt_id = vm.register_native_call(subtle_decrypt_native)?;
  let subtle_sign_id = vm.register_native_call(subtle_sign_native)?;
  let subtle_verify_id = vm.register_native_call(subtle_verify_native)?;
  let subtle_import_key_id = vm.register_native_call(subtle_import_key_native)?;
  let subtle_export_key_id = vm.register_native_call(subtle_export_key_native)?;

  let subtle_fn_slots = [Value::Number(realm_id.to_raw() as f64)];
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
    let call_id = match name {
      "digest" => subtle_digest_id,
      "encrypt" => subtle_encrypt_id,
      "decrypt" => subtle_decrypt_id,
      "sign" => subtle_sign_id,
      "verify" => subtle_verify_id,
      "importKey" => subtle_import_key_id,
      "exportKey" => subtle_export_key_id,
      _ => subtle_unimpl_id,
    };
    let func = scope.alloc_native_function_with_slots(call_id, None, name_s, arity, &subtle_fn_slots)?;
    scope
      .heap_mut()
      .object_set_prototype(func, Some(func_proto))?;
    set_own_data_prop(
      &mut scope,
      subtle_obj,
      name,
      Value::Object(func),
      /* writable */ true,
    )?;
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
  scope.define_property(
    global,
    crypto_key,
    read_only_data_desc(Value::Object(crypto_obj)),
  )?;

  let crypto_ctor_key = alloc_key(&mut scope, "Crypto")?;
  scope.define_property(
    global,
    crypto_ctor_key,
    data_desc(Value::Object(crypto_ctor), true),
  )?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use vm_js::{GcString, Value};

  fn js_value_to_utf8(heap: &Heap, v: Value) -> String {
    match v {
      Value::String(s) => heap.get_string(s).map(|s| s.to_utf8_lossy()).unwrap_or_default(),
      Value::Undefined => "undefined".to_string(),
      Value::Null => "null".to_string(),
      Value::Bool(b) => b.to_string(),
      Value::Number(n) => n.to_string(),
      Value::BigInt(b) => format!("{b:?}"),
      Value::Object(_) => "[object]".to_string(),
      Value::Symbol(_) => "[symbol]".to_string(),
    }
  }

  fn get_string(heap: &Heap, s: GcString) -> String {
    heap.get_string(s).map(|s| s.to_utf8_lossy()).unwrap_or_default()
  }

  #[test]
  fn crypto_get_random_values_type_error_for_non_typed_array() {
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/").with_crypto_rng_seed(1),
    )
    .expect("create realm");

    // Primitive input.
    let v = realm
      .exec_script(
        r#"
        (() => {
          try { crypto.getRandomValues(123); return "no-error"; }
          catch (e) { return e && e.name || String(e); }
        })()
        "#,
      )
      .expect("script should catch and return");

    assert_eq!(js_value_to_utf8(realm.heap(), v), "TypeError");

    // ArrayBuffer and DataView are not accepted.
    let v = realm
      .exec_script(
        r#"
        (() => {
          try { crypto.getRandomValues(new ArrayBuffer(4)); return "no-error"; }
          catch (e) { return e && e.name || String(e); }
        })()
        "#,
      )
      .expect("script should catch and return");
    assert_eq!(js_value_to_utf8(realm.heap(), v), "TypeError");

    let v = realm
      .exec_script(
        r#"
        (() => {
          try { crypto.getRandomValues(new DataView(new ArrayBuffer(4))); return "no-error"; }
          catch (e) { return e && e.name || String(e); }
        })()
        "#,
      )
      .expect("script should catch and return");
    assert_eq!(js_value_to_utf8(realm.heap(), v), "TypeError");

    // Plain object with TypedArray-like shape should still be rejected (brand check).
    let v = realm
      .exec_script(
        r#"
        (() => {
          const fake = {
            constructor: { name: "Uint8Array" },
            buffer: new ArrayBuffer(4),
            byteOffset: 0,
            byteLength: 4,
            length: 4,
          };
          try { crypto.getRandomValues(fake); return "no-error"; }
          catch (e) { return e && e.name || String(e); }
        })()
        "#,
      )
      .expect("script should catch and return");
    assert_eq!(js_value_to_utf8(realm.heap(), v), "TypeError");
  }

  #[test]
  fn crypto_get_random_values_enforces_quota() {
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/").with_crypto_rng_seed(1),
    )
    .expect("create realm");

    let v = realm
      .exec_script(
        r#"
        (() => {
          try { crypto.getRandomValues(new Uint8Array(65537)); return "no-error"; }
          catch (e) { return e && e.name || String(e); }
        })()
        "#,
      )
      .expect("script should catch and return");

    assert_eq!(js_value_to_utf8(realm.heap(), v), "QuotaExceededError");
  }

  #[test]
  fn crypto_deterministic_get_random_values_and_random_uuid_with_fixed_seed() {
    {
      let mut realm = WindowRealm::new(
        WindowRealmConfig::new("https://example.invalid/").with_crypto_rng_seed(1),
      )
      .expect("create realm");

      let value = realm
        .exec_script(
          "(() => { const a = new Uint8Array(16); crypto.getRandomValues(a); return a; })()",
        )
        .expect("getRandomValues");

      let Value::Object(obj) = value else {
        panic!("expected Uint8Array object, got {value:?}");
      };

      let bytes = realm
        .heap()
        .uint8_array_data(obj)
        .expect("uint8_array_data");
      assert_eq!(
        bytes,
        &[
          29, 221, 108, 137, 75, 206, 228, 71, 29, 101, 121, 224, 168, 166, 207, 171
        ][..]
      );
    }

    {
      let mut realm = WindowRealm::new(
        WindowRealmConfig::new("https://example.invalid/").with_crypto_rng_seed(1),
      )
      .expect("create realm");

      let value = realm.exec_script("crypto.randomUUID()").expect("randomUUID");
      let Value::String(s) = value else {
        panic!("expected string, got {value:?}");
      };
      assert_eq!(
        get_string(realm.heap(), s),
        "1ddd6c89-4bce-4447-9d65-79e0a8a6cfab"
      );
    }
  }

  fn install_typed_array_polyfills(realm: &mut WindowRealm) {
    realm
      .exec_script(
        r#"
        (() => {
          function initView(obj, n, bytesPerElement) {
            obj.buffer = new ArrayBuffer(n * bytesPerElement);
            obj.byteOffset = 0;
            obj.byteLength = n * bytesPerElement;
            obj.length = n;
          }

          // `vm-js` only implements `%Uint8Array%` currently; define minimal TypedArray-like
          // constructors for unit tests (only when missing).
          if (typeof Int8Array !== 'function') {
            globalThis.Int8Array = function Int8Array(n) { initView(this, n, 1); };
          }
          if (typeof Uint8ClampedArray !== 'function') {
            globalThis.Uint8ClampedArray = function Uint8ClampedArray(n) { initView(this, n, 1); };
          }
          if (typeof Int16Array !== 'function') {
            globalThis.Int16Array = function Int16Array(n) { initView(this, n, 2); };
          }
          if (typeof Uint16Array !== 'function') {
            globalThis.Uint16Array = function Uint16Array(n) { initView(this, n, 2); };
          }
          if (typeof Int32Array !== 'function') {
            globalThis.Int32Array = function Int32Array(n) { initView(this, n, 4); };
          }
          if (typeof Uint32Array !== 'function') {
            globalThis.Uint32Array = function Uint32Array(n) { initView(this, n, 4); };
          }

          if (typeof Float32Array !== 'function') {
            globalThis.Float32Array = function Float32Array(n) { initView(this, n, 4); };
          }
          return true;
        })()
        "#,
      )
      .expect("install typed array polyfills");
  }

  #[test]
  fn crypto_get_random_values_accepts_integer_typed_arrays() {
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/").with_crypto_rng_seed(1),
    )
    .expect("create realm");
    install_typed_array_polyfills(&mut realm);

    for ta in [
      "Int8Array",
      "Uint8Array",
      "Uint8ClampedArray",
      "Int16Array",
      "Uint16Array",
      "Int32Array",
      "Uint32Array",
    ] {
      let src = format!(
        r#"
        (() => {{
          const a = new {ta}(4);
          const b = crypto.getRandomValues(a);
          return b === a && b.length === 4;
        }})()
        "#
      );
      let v = realm.exec_script(&src).expect("script should run");
      assert_eq!(v, Value::Bool(true), "TA={ta}");
    }
  }

  #[test]
  fn crypto_get_random_values_rejects_float32_array() {
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/").with_crypto_rng_seed(1),
    )
    .expect("create realm");
    install_typed_array_polyfills(&mut realm);

    // Plain Float32Array must be rejected.
    let v = realm
      .exec_script(
        r#"
        (() => {
          try { crypto.getRandomValues(new Float32Array(4)); return "no-error"; }
          catch (e) { return e && e.name || String(e); }
        })()
        "#,
      )
      .expect("script should catch and return");
    assert_eq!(js_value_to_utf8(realm.heap(), v), "TypeError");

    // Spoofing constructor/name should not bypass the integer TypedArray restriction.
    let v = realm
      .exec_script(
        r#"
        (() => {
          const a = new Float32Array(4);
          // `constructor` is inherited and writable on TypedArray prototypes; shadow it.
          a.constructor = { name: "Int32Array" };
          try { crypto.getRandomValues(a); return "no-error"; }
          catch (e) { return e && e.name || String(e); }
        })()
        "#,
      )
      .expect("script should catch and return");
    assert_eq!(js_value_to_utf8(realm.heap(), v), "TypeError");
  }

  #[test]
  fn crypto_get_random_values_enforces_quota_in_bytes() {
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/").with_crypto_rng_seed(1),
    )
    .expect("create realm");
    install_typed_array_polyfills(&mut realm);

    let v = realm
      .exec_script(
        r#"
        (() => {
          try { crypto.getRandomValues(new Uint8Array(65537)); return "no-error"; }
          catch (e) { return e && e.name || String(e); }
        })()
        "#,
      )
      .expect("script should catch and return");
    assert_eq!(js_value_to_utf8(realm.heap(), v), "QuotaExceededError");

    // Quota is enforced on the view's *byteLength*, not element length.
    let v = realm
      .exec_script(
        r#"
        (() => {
          try { crypto.getRandomValues(new Uint32Array(16385)); return "no-error"; }
          catch (e) { return e && e.name || String(e); }
        })()
        "#,
      )
      .expect("script should catch and return");
    assert_eq!(js_value_to_utf8(realm.heap(), v), "QuotaExceededError");
  }

  #[test]
  fn crypto_get_random_values_respects_view_byte_offset_and_length() {
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/").with_crypto_rng_seed(1),
    )
    .expect("create realm");

    assert_eq!(
      realm
        .exec_script(
          r#"
          (() => {
            const buf = new ArrayBuffer(16);
            const u8 = new Uint8Array(buf);
            for (let i = 0; i < u8.length; i++) u8[i] = 0;

            // View covers bytes [4, 12).
            const view = new Uint32Array(buf, 4, 2);
            crypto.getRandomValues(view);

            // Bytes outside the view must remain unchanged (zero).
            for (let i = 0; i < 4; i++) { if (u8[i] !== 0) return false; }
            for (let i = 12; i < 16; i++) { if (u8[i] !== 0) return false; }

            // And at least one byte inside the view should change.
            let anyNonZero = false;
            for (let i = 4; i < 12; i++) { if (u8[i] !== 0) { anyNonZero = true; break; } }
            return anyNonZero;
          })()
          "#,
        )
        .expect("script should run"),
      Value::Bool(true)
    );
  }

  #[test]
  fn crypto_get_random_values_ignores_spoofed_byte_length_property() {
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/").with_crypto_rng_seed(1),
    )
    .expect("create realm");

    // `crypto.getRandomValues` must use the TypedArray's internal slots, not a user-defined
    // `byteLength` data property shadowing the prototype getter.
    let v = realm
      .exec_script(
        r#"
        (() => {
          const a = new Uint8Array(4);
          Object.defineProperty(a, "byteLength", { value: 65537 });
          try { crypto.getRandomValues(a); return "ok"; }
          catch (e) { return e && e.name || String(e); }
        })()
        "#,
      )
      .expect("script should catch and return");
    assert_eq!(js_value_to_utf8(realm.heap(), v), "ok");
  }

  fn assert_sha256_abc_digest(realm: &WindowRealm, ab_obj: vm_js::GcObject) {
    let bytes = realm
      .heap()
      .array_buffer_data(ab_obj)
      .expect("ArrayBuffer data");
    assert_eq!(
      bytes,
      &[
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
        0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
        0xf2, 0x00, 0x15, 0xad,
      ][..]
    );
  }

  #[test]
  fn subtle_digest_sha256_text_encoder_bytes_resolves_to_known_digest() {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))
      .expect("create realm");

    realm
      .exec_script(
        r#"
        globalThis.__digest_done = false;
        globalThis.__digest_ab = null;
        (async () => {
          const data = new TextEncoder().encode('abc');
          const ab = await crypto.subtle.digest('SHA-256', data);
          globalThis.__digest_ab = ab;
          globalThis.__digest_done = true;
        })();
        "#,
      )
      .unwrap();
    realm.perform_microtask_checkpoint().unwrap();

    assert_eq!(
      realm.exec_script("globalThis.__digest_done").unwrap(),
      Value::Bool(true)
    );
    let ab_val = realm.exec_script("globalThis.__digest_ab").unwrap();
    let Value::Object(ab_obj) = ab_val else {
      panic!("expected ArrayBuffer, got {ab_val:?}");
    };
    assert_sha256_abc_digest(&realm, ab_obj);
  }

  #[test]
  fn subtle_digest_accepts_dataview_with_byte_offset() {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))
      .expect("create realm");

    realm
      .exec_script(
        r#"
        globalThis.__digest_done = false;
        globalThis.__digest_ab = null;
        (async () => {
          const u8 = new TextEncoder().encode('xabcx');
          const dv = new DataView(u8.buffer, 1, 3);
          const ab = await crypto.subtle.digest('SHA-256', dv);
          globalThis.__digest_ab = ab;
          globalThis.__digest_done = true;
        })();
        "#,
      )
      .unwrap();
    realm.perform_microtask_checkpoint().unwrap();

    assert_eq!(
      realm.exec_script("globalThis.__digest_done").unwrap(),
      Value::Bool(true)
    );
    let ab_val = realm.exec_script("globalThis.__digest_ab").unwrap();
    let Value::Object(ab_obj) = ab_val else {
      panic!("expected ArrayBuffer, got {ab_val:?}");
    };
    assert_sha256_abc_digest(&realm, ab_obj);
  }

  #[test]
  fn subtle_digest_accepts_uint16array_and_respects_view_byte_offset() {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))
      .expect("create realm");

    realm
      .exec_script(
        r#"
        globalThis.__digest_done = false;
        globalThis.__digest_ab = null;
        (async () => {
          const buf = new ArrayBuffer(6);
          const u8 = new Uint8Array(buf);
          u8[0] = 0x00; u8[1] = 0x01; u8[2] = 0x02; u8[3] = 0x03; u8[4] = 0x04; u8[5] = 0x05;
          // View covers bytes [2, 6).
          const u16 = new Uint16Array(buf, 2, 2);
          const ab = await crypto.subtle.digest('SHA-256', u16);
          globalThis.__digest_ab = ab;
          globalThis.__digest_done = true;
        })();
        "#,
      )
      .unwrap();
    realm.perform_microtask_checkpoint().unwrap();

    assert_eq!(
      realm.exec_script("globalThis.__digest_done").unwrap(),
      Value::Bool(true)
    );
    let ab_val = realm.exec_script("globalThis.__digest_ab").unwrap();
    let Value::Object(ab_obj) = ab_val else {
      panic!("expected ArrayBuffer, got {ab_val:?}");
    };
    let bytes = realm
      .heap()
      .array_buffer_data(ab_obj)
      .expect("ArrayBuffer data");
    let expected = Sha256::digest(&[0x02, 0x03, 0x04, 0x05]).to_vec();
    assert_eq!(bytes, expected.as_slice());
  }

  #[test]
  fn subtle_digest_accepts_algorithm_object_name_via_prototype_getter() {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))
      .expect("create realm");

    realm
      .exec_script(
        r#"
        globalThis.__digest_done = false;
        globalThis.__digest_ab = null;
        (async () => {
          const alg = Object.create({ get name() { return 'SHA-256'; } });
          const data = new TextEncoder().encode('abc');
          const ab = await crypto.subtle.digest(alg, data);
          globalThis.__digest_ab = ab;
          globalThis.__digest_done = true;
        })();
        "#,
      )
      .unwrap();
    realm.perform_microtask_checkpoint().unwrap();

    assert_eq!(
      realm.exec_script("globalThis.__digest_done").unwrap(),
      Value::Bool(true)
    );
    let ab_val = realm.exec_script("globalThis.__digest_ab").unwrap();
    let Value::Object(ab_obj) = ab_val else {
      panic!("expected ArrayBuffer, got {ab_val:?}");
    };
    assert_sha256_abc_digest(&realm, ab_obj);
  }

  #[test]
  fn subtle_digest_accepts_algorithm_object_name_via_getter_to_string() {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))
      .expect("create realm");

    realm
      .exec_script(
        r#"
        globalThis.__digest_done = false;
        globalThis.__digest_ab = null;
        (async () => {
          const alg = Object.create({ get name() { return new String('SHA-256'); } });
          const data = new TextEncoder().encode('abc');
          const ab = await crypto.subtle.digest(alg, data);
          globalThis.__digest_ab = ab;
          globalThis.__digest_done = true;
        })();
        "#,
      )
      .unwrap();
    realm.perform_microtask_checkpoint().unwrap();

    assert_eq!(
      realm.exec_script("globalThis.__digest_done").unwrap(),
      Value::Bool(true)
    );
    let ab_val = realm.exec_script("globalThis.__digest_ab").unwrap();
    let Value::Object(ab_obj) = ab_val else {
      panic!("expected ArrayBuffer, got {ab_val:?}");
    };
    assert_sha256_abc_digest(&realm, ab_obj);
  }

  #[test]
  fn subtle_digest_unsupported_algorithm_rejects_with_not_supported_error() {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))
      .expect("create realm");

    realm
      .exec_script(
        r#"
        globalThis.__digest_done = false;
        globalThis.__digest_err_name = null;
        (async () => {
          try {
            await crypto.subtle.digest('MD5', new TextEncoder().encode('abc'));
          } catch (e) {
            globalThis.__digest_err_name = e && e.name;
          }
          globalThis.__digest_done = true;
        })();
        "#,
      )
      .unwrap();
    realm.perform_microtask_checkpoint().unwrap();

    assert_eq!(
      realm.exec_script("globalThis.__digest_done").unwrap(),
      Value::Bool(true)
    );
    let err_name = realm.exec_script("globalThis.__digest_err_name").unwrap();
    assert_eq!(js_value_to_utf8(realm.heap(), err_name), "NotSupportedError");
  }

  #[test]
  fn subtle_digest_rejects_oversize_input_with_range_error() {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))
      .expect("create realm");

    let src = format!(
      r#"
      globalThis.__digest_done = false;
      globalThis.__digest_err_name = null;
      (async () => {{
        try {{
          const data = new Uint8Array({});
          await crypto.subtle.digest('SHA-256', data);
        }} catch (e) {{
          globalThis.__digest_err_name = e && e.name;
        }}
        globalThis.__digest_done = true;
      }})();
      "#,
      MAX_DIGEST_INPUT_BYTES + 1
    );

    realm.exec_script(&src).unwrap();
    realm.perform_microtask_checkpoint().unwrap();

    assert_eq!(
      realm.exec_script("globalThis.__digest_done").unwrap(),
      Value::Bool(true)
    );
    let err_name = realm.exec_script("globalThis.__digest_err_name").unwrap();
    assert_eq!(js_value_to_utf8(realm.heap(), err_name), "RangeError");
  }

  #[test]
  fn subtle_unimplemented_methods_return_promise_and_reject() {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))
      .expect("create realm");

    // Must not throw synchronously.
    assert_eq!(
      realm
        .exec_script(
          r#"
          (() => {
            try {
              const p = crypto.subtle.encrypt('nope', null, new Uint8Array([1,2,3]));
              return p instanceof Promise;
            } catch (e) {
              return false;
            }
          })()
          "#,
        )
        .unwrap(),
      Value::Bool(true)
    );

    // And the returned Promise must reject.
    realm
      .exec_script(
        r#"
        globalThis.__encrypt_done = false;
        globalThis.__encrypt_outcome = null;
        (async () => {
          try {
            await crypto.subtle.encrypt('nope', null, new Uint8Array([1,2,3]));
            globalThis.__encrypt_outcome = 'resolved';
          } catch (e) {
            globalThis.__encrypt_outcome = 'rejected';
          }
          globalThis.__encrypt_done = true;
        })();
        "#,
      )
      .unwrap();
    realm.perform_microtask_checkpoint().unwrap();

    assert_eq!(
      realm.exec_script("globalThis.__encrypt_done").unwrap(),
      Value::Bool(true)
    );
    let outcome = realm.exec_script("globalThis.__encrypt_outcome").unwrap();
    assert_eq!(js_value_to_utf8(realm.heap(), outcome), "rejected");
  }

  #[test]
  fn subtle_crypto_object_to_string_tag_is_branded() {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))
      .expect("create realm");

    let v = realm
      .exec_script("Object.prototype.toString.call(crypto.subtle)")
      .unwrap();
    assert_eq!(js_value_to_utf8(realm.heap(), v), "[object SubtleCrypto]");
  }
}
