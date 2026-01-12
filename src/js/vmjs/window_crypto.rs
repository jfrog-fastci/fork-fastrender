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
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};
use vm_js::{
  new_promise_capability_with_host_and_hooks, new_type_error_object, GcObject, Heap,
  PromiseCapability, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm,
  VmError, VmHost, VmHostHooks,
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

  let algorithm_name: Option<String> = match algorithm {
    Value::String(s) => Some(scope.heap().get_string(s)?.to_utf8_lossy()),
    Value::Object(obj) => {
      scope.push_root(Value::Object(obj))?;
      let name_key_s = scope.alloc_string("name")?;
      scope.push_root(Value::String(name_key_s))?;
      let name_key = PropertyKey::from_string(name_key_s);
      match scope
        .heap()
        .object_get_own_data_property_value(obj, &name_key)?
        .unwrap_or(Value::Undefined)
      {
        Value::String(s) => Some(scope.heap().get_string(s)?.to_utf8_lossy()),
        _ => None,
      }
    }
    _ => None,
  };

  enum DigestAlg {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
  }

  let digest_alg: Option<DigestAlg> = algorithm_name.as_deref().and_then(|name| {
    let name = name.trim();
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
  });

  let data_bytes: Option<Vec<u8>> = match data {
    Value::Object(obj) => {
      scope.push_root(Value::Object(obj))?;
      let heap = scope.heap();
      if heap.is_array_buffer_object(obj) {
        Some(heap.array_buffer_data(obj)?.to_vec())
      } else if heap.is_uint8_array_object(obj) {
        Some(heap.uint8_array_data(obj)?.to_vec())
      } else {
        None
      }
    }
    _ => None,
  };

  let result: Result<Vec<u8>, Value> = match (algorithm_name.as_deref(), digest_alg, data_bytes) {
    (Some(_name), Some(alg), Some(data)) => {
      let digest = match alg {
        DigestAlg::Sha1 => Sha1::digest(&data).to_vec(),
        DigestAlg::Sha256 => Sha256::digest(&data).to_vec(),
        DigestAlg::Sha384 => Sha384::digest(&data).to_vec(),
        DigestAlg::Sha512 => Sha512::digest(&data).to_vec(),
      };
      Ok(digest)
    }
    (None, _, _) => Err(new_type_error_object(
      &mut scope,
      &intr,
      "crypto.subtle.digest expects an algorithm name string",
    )?),
    (Some(name), None, _) => {
      let message = format!("Unsupported digest algorithm: {name}");
      Err(create_dom_exception_like(
        &mut scope,
        "NotSupportedError",
        &message,
      )?)
    }
    (_, _, None) => Err(new_type_error_object(
      &mut scope,
      &intr,
      "crypto.subtle.digest expects an ArrayBuffer or Uint8Array",
    )?),
  };

  match result {
    Ok(digest) => {
      let ab = scope.alloc_array_buffer_from_u8_vec(digest)?;
      scope
        .heap_mut()
        .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
      vm.call_with_host_and_hooks(
        &mut *host,
        &mut scope,
        &mut *hooks,
        cap.resolve,
        Value::Undefined,
        &[Value::Object(ab)],
      )?;
    }
    Err(err_value) => {
      vm.call_with_host_and_hooks(
        &mut *host,
        &mut scope,
        &mut *hooks,
        cap.reject,
        Value::Undefined,
        &[err_value],
      )?;
    }
  }

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
  let subtle_digest_id = vm.register_native_call(subtle_digest_native)?;
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
    let call_id = if name == "digest" {
      subtle_digest_id
    } else {
      subtle_unimpl_id
    };
    let func = scope.alloc_native_function(call_id, None, name_s, arity)?;
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
}
