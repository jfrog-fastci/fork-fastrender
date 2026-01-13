//! WHATWG `URL` + `URLSearchParams` bindings for the `vm-js` script runtime (`WindowRealm`).
//!
//! `URL`/`URLSearchParams` wrappers need per-object Rust state (the parsed URL + query list) and
//! `vm-js` native call hooks do not currently provide a convenient per-realm host state slot.
//! Instead, the bindings store wrapper state in a process-global weak registry keyed by the
//! `RealmId` plus the wrapper object's `WeakGcObject` handle.
//!
//! The registry is swept opportunistically whenever the heap's GC run counter changes.

use crate::js::{Url, UrlError, UrlLimits, UrlSearchParams};
use crate::js::{window_blob, window_object_url};
use crate::js::window_realm::WindowRealmUserData;
use std::char::decode_utf16;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use vm_js::iterator;
use vm_js::{
  GcObject, GcString, Heap, HostSlots, PropertyDescriptor, PropertyKey, PropertyKind, Realm,
  RealmId, RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};

const ILLEGAL_CONSTRUCTOR_ERROR: &str = "Illegal constructor";
const URL_INVALID_ERROR: &str = "Invalid URL";
const URL_INPUT_TOO_LONG_ERROR: &str = "URL constructor input exceeded max bytes";
const URL_BASE_TOO_LONG_ERROR: &str = "URL constructor base exceeded max bytes";
const URLSP_INIT_TOO_LONG_ERROR: &str = "URLSearchParams constructor init exceeded max bytes";
const URLSP_ARG_TOO_LONG_ERROR: &str = "URLSearchParams argument exceeded max bytes";

// Object URL (blob:) errors.
//
// Note: we currently surface quota failures as `TypeError` to avoid threading a `DOMException`
// constructor handle through these bindings.
const OBJECT_URL_BLOB_REQUIRED_ERROR: &str = "URL.createObjectURL requires a Blob";
const OBJECT_URL_QUOTA_EXCEEDED_ERROR: &str = "URL.createObjectURL exceeded object URL limits";

// `URL.revokeObjectURL()` is specified as a no-op for unknown URLs. Cap the size of strings we are
// willing to convert into a Rust `String` so untrusted scripts cannot force large Rust allocations.
const REVOKE_OBJECT_URL_MAX_CODE_UNITS: usize = 8 * 1024;
const REVOKE_OBJECT_URL_MAX_UTF8_BYTES: usize = REVOKE_OBJECT_URL_MAX_CODE_UNITS * 3;
// Brand `URL`/`URLSearchParams` wrappers as platform objects via HostSlots so structuredClone rejects
// them with DataCloneError.
const URL_HOST_TAG: u64 = 0x5552_4C5F_5F5F_5F5F; // "URL_____"
const URL_SEARCH_PARAMS_HOST_TAG: u64 = 0x5552_4C53_5041_5253; // "URLSPARS"
const URL_SEARCH_PARAMS_ITERATOR_HOST_TAG: u64 = 0x5552_4C53_5049_5452; // "URLSPITR"

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn idl_data_desc(value: Value) -> PropertyDescriptor {
  // WebIDL interface members are enumerable by default for string-named properties.
  // (Symbol-named members like @@iterator are typically non-enumerable.)
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn proto_data_desc(value: Value) -> PropertyDescriptor {
  // Prototype properties are usually non-enumerable, writable, configurable.
  data_desc(value)
}

fn ctor_link_desc(value: Value) -> PropertyDescriptor {
  // `prototype` and `constructor` links are typically non-enumerable.
  PropertyDescriptor {
    enumerable: false,
    configurable: false,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
}

fn accessor_desc(get: Value, set: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Accessor { get, set },
  }
}

fn idl_accessor_desc(get: Value, set: Value) -> PropertyDescriptor {
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

fn prototype_from_new_target(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  new_target: Value,
  fallback: GcObject,
) -> Result<GcObject, VmError> {
  let Value::Object(ctor) = new_target else {
    return Ok(fallback);
  };

  // `new_target.prototype` is ordinary property access and can invoke user code (getters/proxies), so
  // callers must ensure they are not holding any non-reentrant locks.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(ctor))?;
  scope.push_root(Value::Object(fallback))?;

  let prototype_key = alloc_key(&mut scope, "prototype")?;
  let proto_value = vm.get_with_host_and_hooks(host, &mut scope, hooks, ctor, prototype_key)?;
  match proto_value {
    Value::Object(obj) => Ok(obj),
    _ => Ok(fallback),
  }
}

const URLSP_ITER_KIND_ENTRIES: u8 = 0;
const URLSP_ITER_KIND_KEYS: u8 = 1;
const URLSP_ITER_KIND_VALUES: u8 = 2;

struct UrlSearchParamsIteratorState {
  params: UrlSearchParams,
  index: usize,
  kind: u8,
}

struct CachedParamsEntry {
  params_obj: GcObject,
  params_root: RootId,
}

#[derive(Default)]
struct UrlRegistry {
  realms: HashMap<RealmId, UrlRealmState>,
}

struct UrlRealmState {
  limits: UrlLimits,
  url_proto: GcObject,
  params_proto: GcObject,
  urls: HashMap<WeakGcObject, Url>,
  params: HashMap<WeakGcObject, UrlSearchParams>,
  params_iterators: HashMap<WeakGcObject, UrlSearchParamsIteratorState>,
  cached_search_params: HashMap<WeakGcObject, CachedParamsEntry>,
  last_gc_runs: u64,
  last_gc_runs_cached_search_params: u64,
}

static REGISTRY: OnceLock<Mutex<UrlRegistry>> = OnceLock::new();

fn registry() -> &'static Mutex<UrlRegistry> {
  REGISTRY.get_or_init(|| Mutex::new(UrlRegistry::default()))
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
  let realm_id =
    slots
      .get(0)
      .copied()
      .and_then(realm_id_from_slot)
      .ok_or(VmError::InvariantViolation(
        "URL bindings invoked without an active realm",
      ))?;
  Ok(realm_id)
}

fn with_realm_state_mut<R>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  f: impl FnOnce(&mut Vm, &mut UrlRealmState, &mut Scope<'_>) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let realm_id = realm_id_for_binding_call(vm, scope, callee)?;

  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  let state = registry
    .realms
    .get_mut(&realm_id)
    .ok_or(VmError::InvariantViolation(
      "URL bindings used before install_window_url_bindings",
    ))?;

  // Opportunistically sweep dead wrappers when GC has run.
  let gc_runs = scope.heap().gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    {
      let heap = scope.heap();
      state.urls.retain(|k, _| k.upgrade(heap).is_some());
      state.params.retain(|k, _| k.upgrade(heap).is_some());
      state
        .params_iterators
        .retain(|k, _| k.upgrade(heap).is_some());
    }
  }

  if gc_runs != state.last_gc_runs_cached_search_params {
    state.last_gc_runs_cached_search_params = gc_runs;

    // Drop cached `URL.searchParams` objects once their `URL` wrapper is dead.
    //
    // Each cached `URLSearchParams` wrapper is held live by a heap root so it behaves like a spec
    // internal slot rather than an observable property. When the corresponding `URL` wrapper is GC'd
    // we must remove the root to allow the params wrapper to be collected if not otherwise
    // referenced.
    let heap = scope.heap_mut();
    state.cached_search_params.retain(|k, entry| {
      if k.upgrade(&*heap).is_some() {
        true
      } else {
        heap.remove_root(entry.params_root);
        false
      }
    });
  }

  f(vm, state, scope)
}

fn require_url(scope: &Scope<'_>, state: &UrlRealmState, this: Value) -> Result<Url, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let slots = scope
    .heap()
    .object_host_slots(obj)?
    .ok_or(VmError::TypeError("Illegal invocation"))?;
  if slots.a != URL_HOST_TAG {
    return Err(VmError::TypeError("Illegal invocation"));
  }
  state
    .urls
    .get(&WeakGcObject::from(obj))
    .cloned()
    .ok_or(VmError::TypeError("Illegal invocation"))
}

fn require_params(
  scope: &Scope<'_>,
  state: &UrlRealmState,
  this: Value,
) -> Result<UrlSearchParams, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let slots = scope
    .heap()
    .object_host_slots(obj)?
    .ok_or(VmError::TypeError("Illegal invocation"))?;
  if slots.a != URL_SEARCH_PARAMS_HOST_TAG {
    return Err(VmError::TypeError("Illegal invocation"));
  }
  state
    .params
    .get(&WeakGcObject::from(obj))
    .cloned()
    .ok_or(VmError::TypeError("Illegal invocation"))
}

fn js_string_to_rust_string_limited(
  scope: &mut Scope<'_>,
  handle: GcString,
  max_bytes: usize,
  err: &'static str,
) -> Result<String, VmError> {
  let js = scope.heap().get_string(handle)?;

  // UTF-8 bytes are always >= UTF-16 code unit count. Use this to reject pathological strings
  // without iterating.
  let code_units_len = js.len_code_units();
  if code_units_len > max_bytes {
    return Err(VmError::TypeError(err));
  }

  let capacity = code_units_len.saturating_mul(3).min(max_bytes);
  let mut out = String::new();
  out
    .try_reserve_exact(capacity)
    .map_err(|_| VmError::OutOfMemory)?;
  let mut written: usize = 0;

  for decoded in decode_utf16(js.as_code_units().iter().copied()) {
    let ch = decoded.unwrap_or('\u{FFFD}');
    let len = ch.len_utf8();
    let next = written.saturating_add(len);
    if next > max_bytes {
      return Err(VmError::TypeError(err));
    }
    out.push(ch);
    written = next;
  }

  Ok(out)
}

fn value_to_limited_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  max_bytes: usize,
  err: &'static str,
) -> Result<String, VmError> {
  let s: GcString = scope.to_string(vm, host, hooks, value)?;
  js_string_to_rust_string_limited(scope, s, max_bytes, err)
}

fn serialized_origin_for_document_url(url: &str) -> String {
  let Ok(url) = ::url::Url::parse(url) else {
    return "null".to_string();
  };
  match url.scheme() {
    "http" | "https" => url.origin().ascii_serialization(),
    _ => "null".to_string(),
  }
}

fn current_realm_serialized_origin(vm: &mut Vm) -> String {
  let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
    return "null".to_string();
  };
  serialized_origin_for_document_url(data.document_url())
}

fn map_url_error(err: UrlError) -> VmError {
  match err {
    UrlError::OutOfMemory => VmError::OutOfMemory,
    // For now we surface size-limit failures as TypeError to keep error construction simple.
    UrlError::LimitExceeded { .. } => VmError::TypeError("URL exceeds size limits"),
    UrlError::InvalidUtf8 => VmError::TypeError(URL_INVALID_ERROR),
    UrlError::ParseError | UrlError::InvalidBase { .. } | UrlError::Parse { .. } => {
      VmError::TypeError(URL_INVALID_ERROR)
    }
    UrlError::SetterFailure { setter, .. } => {
      if matches!(setter, crate::resource::web_url::WebUrlSetter::Href) {
        VmError::TypeError(URL_INVALID_ERROR)
      } else {
        // WHATWG URL setters (other than `href`) are "no-op on failure".
        // Represented by returning a sentinel error that callers can ignore.
        VmError::TypeError("URL setter failed")
      }
    }
  }
}

fn ignore_setter_failure(result: Result<(), UrlError>) -> Result<(), VmError> {
  match result {
    Ok(()) => Ok(()),
    Err(UrlError::SetterFailure { .. }) => Ok(()),
    Err(e) => Err(map_url_error(e)),
  }
}

fn install_method(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  realm: &Realm,
  proto: GcObject,
  name: &str,
  call: vm_js::NativeCall,
  length: u32,
  realm_slot: Value,
) -> Result<(), VmError> {
  let call_id = vm.register_native_call(call)?;
  let func_name = scope.alloc_string(name)?;
  scope.push_root(Value::String(func_name))?;
  let slots = [realm_slot];
  let func = scope.alloc_native_function_with_slots(call_id, None, func_name, length, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(func))?;
  let key = alloc_key(scope, name)?;
  scope.define_property(proto, key, idl_data_desc(Value::Object(func)))?;
  Ok(())
}

fn install_accessor(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  realm: &Realm,
  proto: GcObject,
  name: &str,
  get_call: vm_js::NativeCall,
  set_call: Option<vm_js::NativeCall>,
  realm_slot: Value,
) -> Result<(), VmError> {
  let get_id = vm.register_native_call(get_call)?;
  let get_name = scope.alloc_string(&format!("get {name}"))?;
  scope.push_root(Value::String(get_name))?;
  let slots = [realm_slot];
  let get_func = scope.alloc_native_function_with_slots(get_id, None, get_name, 0, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(get_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(get_func))?;

  let set_value = if let Some(set_call) = set_call {
    let set_id = vm.register_native_call(set_call)?;
    let set_name = scope.alloc_string(&format!("set {name}"))?;
    scope.push_root(Value::String(set_name))?;
    let set_func = scope.alloc_native_function_with_slots(set_id, None, set_name, 1, &slots)?;
    scope
      .heap_mut()
      .object_set_prototype(set_func, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(set_func))?;
    Value::Object(set_func)
  } else {
    Value::Undefined
  };

  let key = alloc_key(scope, name)?;
  scope.define_property(
    proto,
    key,
    idl_accessor_desc(Value::Object(get_func), set_value),
  )?;
  Ok(())
}

fn url_call_without_new_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let intrinsics = vm
    .intrinsics()
    .ok_or(VmError::InvariantViolation("vm intrinsics not initialized"))?;
  Err(vm_js::throw_type_error(
    scope,
    intrinsics,
    ILLEGAL_CONSTRUCTOR_ERROR,
  ))
}

fn url_construct_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let (limits, fallback_proto) =
    with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
      Ok((state.limits.clone(), state.url_proto))
    })?;

  // WebIDL constructor behavior: `new_target.prototype` is consulted to select the wrapper's
  // prototype, so subclasses like `class X extends URL {}` produce `X` instances.
  //
  // Note: compute this outside `with_realm_state_mut` (which holds the URL registry lock), because
  // property access can execute user code.
  let proto = prototype_from_new_target(vm, scope, host, hooks, new_target, fallback_proto)?;
  scope.push_root(Value::Object(proto))?;

  let input_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let input = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    input_value,
    limits.max_input_bytes,
    URL_INPUT_TOO_LONG_ERROR,
  )?;

  let base = match args.get(1).copied() {
    None | Some(Value::Undefined) => None,
    Some(v) => Some(value_to_limited_string(
      vm,
      scope,
      host,
      hooks,
      v,
      limits.max_input_bytes,
      URL_BASE_TOO_LONG_ERROR,
    )?),
  };

  let url =
    Url::parse_without_diagnostics(&input, base.as_deref(), &limits).map_err(map_url_error)?;

  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let obj = scope.alloc_object()?;
    scope.heap_mut().object_set_prototype(obj, Some(proto))?;
    scope.heap_mut().object_set_host_slots(
      obj,
      HostSlots {
        a: URL_HOST_TAG,
        b: 0,
      },
    )?;
    state.urls.insert(WeakGcObject::from(obj), url);
    Ok(Value::Object(obj))
  })
}

fn url_parse_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let limits = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    Ok(state.limits.clone())
  })?;

  let input_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let input = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    input_value,
    limits.max_input_bytes,
    URL_INPUT_TOO_LONG_ERROR,
  )?;

  let base = match args.get(1).copied() {
    None | Some(Value::Undefined) => None,
    Some(v) => Some(value_to_limited_string(
      vm,
      scope,
      host,
      hooks,
      v,
      limits.max_input_bytes,
      URL_BASE_TOO_LONG_ERROR,
    )?),
  };

  let url = match Url::parse_without_diagnostics(&input, base.as_deref(), &limits) {
    Ok(url) => url,
    Err(UrlError::OutOfMemory) => return Err(VmError::OutOfMemory),
    Err(_) => return Ok(Value::Null),
  };

  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let obj = scope.alloc_object()?;
    scope
      .heap_mut()
      .object_set_prototype(obj, Some(state.url_proto))?;
    scope.heap_mut().object_set_host_slots(
      obj,
      HostSlots {
        a: URL_HOST_TAG,
        b: 0,
      },
    )?;
    state.urls.insert(WeakGcObject::from(obj), url);
    Ok(Value::Object(obj))
  })
}

fn url_can_parse_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let limits = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    Ok(state.limits.clone())
  })?;

  let input_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let input = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    input_value,
    limits.max_input_bytes,
    URL_INPUT_TOO_LONG_ERROR,
  )?;

  let base = match args.get(1).copied() {
    None | Some(Value::Undefined) => None,
    Some(v) => Some(value_to_limited_string(
      vm,
      scope,
      host,
      hooks,
      v,
      limits.max_input_bytes,
      URL_BASE_TOO_LONG_ERROR,
    )?),
  };

  match Url::parse_without_diagnostics(&input, base.as_deref(), &limits) {
    Ok(_) => Ok(Value::Bool(true)),
    Err(UrlError::OutOfMemory) => Err(VmError::OutOfMemory),
    Err(_) => Ok(Value::Bool(false)),
  }
}

fn url_create_object_url_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let blob_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let Some(blob) = window_blob::clone_blob_data_for_fetch(vm, scope.heap(), blob_value)? else {
    return Err(VmError::TypeError(OBJECT_URL_BLOB_REQUIRED_ERROR));
  };

  let origin = current_realm_serialized_origin(vm);
  let url = match window_object_url::create_object_url(&origin, blob.bytes, blob.r#type) {
    Ok(url) => url,
    Err(window_object_url::CreateObjectUrlError::OutOfMemory) => return Err(VmError::OutOfMemory),
    Err(
      window_object_url::CreateObjectUrlError::TooManyUrls
      | window_object_url::CreateObjectUrlError::TooManyBytes,
    ) => return Err(VmError::TypeError(OBJECT_URL_QUOTA_EXCEEDED_ERROR)),
  };
  let s = scope.alloc_string(&url)?;
  Ok(Value::String(s))
}

fn url_revoke_object_url_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let url_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let s: GcString = scope.to_string(vm, host, hooks, url_value)?;
  let code_units_len = scope.heap().get_string(s)?.len_code_units();
  if code_units_len > REVOKE_OBJECT_URL_MAX_CODE_UNITS {
    return Ok(Value::Undefined);
  }

  let url = match js_string_to_rust_string_limited(
    scope,
    s,
    REVOKE_OBJECT_URL_MAX_UTF8_BYTES,
    "URL.revokeObjectURL argument exceeded max bytes",
  ) {
    Ok(url) => url,
    // `revokeObjectURL` should not throw for invalid/unknown URLs.
    Err(_) => return Ok(Value::Undefined),
  };
  window_object_url::revoke_object_url(&url);
  Ok(Value::Undefined)
}

fn url_href_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    let href = url.href().map_err(map_url_error)?;
    let s = scope.alloc_string(&href)?;
    Ok(Value::String(s))
  })
}

fn url_href_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    Ok((url, state.limits.max_input_bytes))
  })?;

  let value = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URL_INPUT_TOO_LONG_ERROR,
  )?;
  url.set_href(&value).map_err(map_url_error)?;
  Ok(Value::Undefined)
}

fn url_origin_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    let origin = url.origin();
    let s = scope.alloc_string(&origin)?;
    Ok(Value::String(s))
  })
}

fn url_protocol_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    let protocol = url.protocol().map_err(map_url_error)?;
    let s = scope.alloc_string(&protocol)?;
    Ok(Value::String(s))
  })
}

fn url_protocol_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    Ok((url, state.limits.max_input_bytes))
  })?;

  let value = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URL_INPUT_TOO_LONG_ERROR,
  )?;
  ignore_setter_failure(url.set_protocol(&value))?;
  Ok(Value::Undefined)
}

fn url_username_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    let username = url.username().map_err(map_url_error)?;
    let s = scope.alloc_string(&username)?;
    Ok(Value::String(s))
  })
}

fn url_username_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    Ok((url, state.limits.max_input_bytes))
  })?;

  let value = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URL_INPUT_TOO_LONG_ERROR,
  )?;
  ignore_setter_failure(url.set_username(&value))?;
  Ok(Value::Undefined)
}

fn urlsp_init_pair_from_sequence(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  limits: &UrlLimits,
  value: Value,
) -> Result<(String, String), VmError> {
  let intrinsics = vm
    .intrinsics()
    .ok_or(VmError::InvariantViolation("vm intrinsics not initialized"))?;
  let mut record = iterator::get_iterator(vm, host, hooks, scope, value)?;

  let Some(name_value) = iterator::iterator_step_value(vm, host, hooks, scope, &mut record)? else {
    if !record.done {
      if let Err(err) = iterator::iterator_close(
        vm,
        host,
        hooks,
        scope,
        &record,
        iterator::CloseCompletionKind::Throw,
      ) {
        return Err(err);
      }
    }
    return Err(vm_js::throw_type_error(
      scope,
      intrinsics,
      "URLSearchParams init pair must contain exactly two values",
    ));
  };
  let Some(value_value) = iterator::iterator_step_value(vm, host, hooks, scope, &mut record)? else {
    if !record.done {
      if let Err(err) = iterator::iterator_close(
        vm,
        host,
        hooks,
        scope,
        &record,
        iterator::CloseCompletionKind::Throw,
      ) {
        return Err(err);
      }
    }
    return Err(vm_js::throw_type_error(
      scope,
      intrinsics,
      "URLSearchParams init pair must contain exactly two values",
    ));
  };
  if iterator::iterator_step_value(vm, host, hooks, scope, &mut record)?.is_some() {
    if !record.done {
      if let Err(err) = iterator::iterator_close(
        vm,
        host,
        hooks,
        scope,
        &record,
        iterator::CloseCompletionKind::Throw,
      ) {
        return Err(err);
      }
    }
    return Err(vm_js::throw_type_error(
      scope,
      intrinsics,
      "URLSearchParams init pair must contain exactly two values",
    ));
  }

  let name = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    name_value,
    limits.max_input_bytes,
    URLSP_INIT_TOO_LONG_ERROR,
  )?;
  let value = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    value_value,
    limits.max_input_bytes,
    URLSP_INIT_TOO_LONG_ERROR,
  )?;
  Ok((name, value))
}

fn urlsp_init_from_iterable(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  limits: &UrlLimits,
  mut record: iterator::IteratorRecord,
) -> Result<UrlSearchParams, VmError> {
  let mut params = UrlSearchParams::new(limits);

  let result = (|| -> Result<(), VmError> {
    while let Some(pair_value) = iterator::iterator_step_value(vm, host, hooks, scope, &mut record)?
    {
      let (name, value) =
        urlsp_init_pair_from_sequence(vm, scope, host, hooks, limits, pair_value)?;
      params.append(&name, &value).map_err(map_url_error)?;
    }
    Ok(())
  })();

  match result {
    Ok(()) => Ok(params),
    Err(err) => {
      if record.done {
        return Err(err);
      }
      // If iterator close throws, it overrides the original error (ECMA-262 `IteratorClose`),
      // but it must not replace VM-internal fatal errors (termination, OOM, etc).
      let original_is_throw = err.is_throw_completion();
      let pending_root = err.thrown_value().map(|v| scope.heap_mut().add_root(v)).transpose()?;
      let close_res = iterator::iterator_close(
        vm,
        host,
        hooks,
        scope,
        &record,
        iterator::CloseCompletionKind::Throw,
      );
      if let Some(root) = pending_root {
        scope.heap_mut().remove_root(root);
      }
      match close_res {
        Ok(()) => Err(err),
        Err(close_err) => {
          if original_is_throw {
            Err(close_err)
          } else {
            Err(err)
          }
        }
      }
    }
  }
}

fn urlsp_init_from_record(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  limits: &UrlLimits,
  obj: GcObject,
) -> Result<UrlSearchParams, VmError> {
  let keys = scope.heap().own_property_keys(obj)?;
  let mut params = UrlSearchParams::new(limits);

  for key in keys {
    let PropertyKey::String(name_key) = key else {
      continue;
    };

    let Some(desc) = scope.heap().get_own_property(obj, key)? else {
      continue;
    };
    if !desc.enumerable {
      continue;
    }

    let name = js_string_to_rust_string_limited(
      scope,
      name_key,
      limits.max_input_bytes,
      URLSP_INIT_TOO_LONG_ERROR,
    )?;
    let value = vm.get_with_host_and_hooks(host, scope, hooks, obj, key)?;
    let value = value_to_limited_string(
      vm,
      scope,
      host,
      hooks,
      value,
      limits.max_input_bytes,
      URLSP_INIT_TOO_LONG_ERROR,
    )?;

    params.append(&name, &value).map_err(map_url_error)?;
  }

  Ok(params)
}

fn url_password_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    let password = url.password().map_err(map_url_error)?;
    let s = scope.alloc_string(&password)?;
    Ok(Value::String(s))
  })
}

fn url_password_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    Ok((url, state.limits.max_input_bytes))
  })?;

  let value = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URL_INPUT_TOO_LONG_ERROR,
  )?;
  ignore_setter_failure(url.set_password(&value))?;
  Ok(Value::Undefined)
}

fn url_host_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    let host = url.host().map_err(map_url_error)?;
    let s = scope.alloc_string(&host)?;
    Ok(Value::String(s))
  })
}

fn url_host_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    Ok((url, state.limits.max_input_bytes))
  })?;

  let value = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URL_INPUT_TOO_LONG_ERROR,
  )?;
  ignore_setter_failure(url.set_host(&value))?;
  Ok(Value::Undefined)
}

fn url_hostname_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    let hostname = url.hostname().map_err(map_url_error)?;
    let s = scope.alloc_string(&hostname)?;
    Ok(Value::String(s))
  })
}

fn url_hostname_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    Ok((url, state.limits.max_input_bytes))
  })?;

  let value = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URL_INPUT_TOO_LONG_ERROR,
  )?;
  ignore_setter_failure(url.set_hostname(&value))?;
  Ok(Value::Undefined)
}

fn url_port_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    let port = url.port().map_err(map_url_error)?;
    let s = scope.alloc_string(&port)?;
    Ok(Value::String(s))
  })
}

fn url_port_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    Ok((url, state.limits.max_input_bytes))
  })?;

  let value = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URL_INPUT_TOO_LONG_ERROR,
  )?;
  ignore_setter_failure(url.set_port(&value))?;
  Ok(Value::Undefined)
}

fn url_pathname_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    let pathname = url.pathname().map_err(map_url_error)?;
    let s = scope.alloc_string(&pathname)?;
    Ok(Value::String(s))
  })
}

fn url_pathname_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    Ok((url, state.limits.max_input_bytes))
  })?;

  let value = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URL_INPUT_TOO_LONG_ERROR,
  )?;
  ignore_setter_failure(url.set_pathname(&value))?;
  Ok(Value::Undefined)
}

fn url_search_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    let search = url.search().map_err(map_url_error)?;
    let s = scope.alloc_string(&search)?;
    Ok(Value::String(s))
  })
}

fn url_search_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    Ok((url, state.limits.max_input_bytes))
  })?;

  let value = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URL_INPUT_TOO_LONG_ERROR,
  )?;
  ignore_setter_failure(url.set_search(&value))?;
  Ok(Value::Undefined)
}

fn url_hash_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    let hash = url.hash().map_err(map_url_error)?;
    let s = scope.alloc_string(&hash)?;
    Ok(Value::String(s))
  })
}

fn url_hash_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    Ok((url, state.limits.max_input_bytes))
  })?;

  let value = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URL_INPUT_TOO_LONG_ERROR,
  )?;
  ignore_setter_failure(url.set_hash(&value))?;
  Ok(Value::Undefined)
}

fn url_search_params_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // We need to cache the `URLSearchParams` wrapper but it should not be observable via reflection
  // (e.g. `Object.getOwnPropertySymbols(url)`), so we store it in host state rather than as a
  // hidden property on the `URL` object.
  //
  // Each cached wrapper is kept alive via a heap root until the `URL` wrapper is collected demonstrating
  // browser-like internal-slot semantics.

  struct CreateInfo {
    url_key: WeakGcObject,
    params_proto: GcObject,
    params: UrlSearchParams,
  }

  enum Lookup {
    Hit(GcObject),
    Miss(CreateInfo),
  }

  let lookup = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let Value::Object(url_obj) = this else {
      return Err(VmError::TypeError("Illegal invocation"));
    };

    // Brand check.
    let slots = scope
      .heap()
      .object_host_slots(url_obj)?
      .ok_or(VmError::TypeError("Illegal invocation"))?;
    if slots.a != URL_HOST_TAG {
      return Err(VmError::TypeError("Illegal invocation"));
    }

    let url_key = WeakGcObject::from(url_obj);
    if let Some(entry) = state.cached_search_params.get(&url_key) {
      return Ok(Lookup::Hit(entry.params_obj));
    }

    let url = state
      .urls
      .get(&WeakGcObject::from(url_obj))
      .cloned()
      .ok_or(VmError::TypeError("Illegal invocation"))?;

    Ok(Lookup::Miss(CreateInfo {
      url_key,
      params_proto: state.params_proto,
      params: url.search_params(),
    }))
  })?;

  match lookup {
    Lookup::Hit(obj) => Ok(Value::Object(obj)),
    Lookup::Miss(CreateInfo {
      url_key,
      params_proto,
      params,
    }) => {
      let params_obj = scope.alloc_object()?;
      scope
        .heap_mut()
        .object_set_prototype(params_obj, Some(params_proto))?;
      scope.heap_mut().object_set_host_slots(
        params_obj,
        HostSlots {
          a: URL_SEARCH_PARAMS_HOST_TAG,
          b: 0,
        },
      )?;

      // Create a per-object persistent root to keep the cached params wrapper alive as long as the
      // URL wrapper is alive.
      //
      // Important: do this outside of the URL registry mutex to avoid deadlocks if adding a root
      // triggers GC.
      let params_root = scope
        .heap_mut()
        .add_root(Value::Object(params_obj))?;

      // Insert into host cache if another access did not win the race while we allocated.
      let inserted = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
        if let Some(entry) = state.cached_search_params.get(&url_key) {
          return Ok(Ok(entry.params_obj));
        }

        state.params.insert(WeakGcObject::from(params_obj), params);
        state.cached_search_params.insert(
          url_key,
          CachedParamsEntry {
            params_obj,
            params_root,
          },
        );
        Ok(Err(params_obj))
      });

      match inserted {
        Ok(Ok(existing)) => {
          // Someone else installed the cache entry while we were allocating. Drop our root so this
          // unreferenced wrapper can be collected.
          scope.heap_mut().remove_root(params_root);
          Ok(Value::Object(existing))
        }
        Ok(Err(created)) => Ok(Value::Object(created)),
        Err(err) => {
          // Ensure we do not leak the root on error.
          scope.heap_mut().remove_root(params_root);
          Err(err)
        }
      }
    }
  }
}

fn url_to_string_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let url = require_url(scope, state, this)?;
    let href = url.href().map_err(map_url_error)?;
    let s = scope.alloc_string(&href)?;
    Ok(Value::String(s))
  })
}

fn url_to_json_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // WHATWG URL `toJSON` is the same as `href`.
  url_to_string_native(vm, scope, _host, _hooks, callee, this, _args)
}

fn urlsp_call_without_new_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let intrinsics = vm
    .intrinsics()
    .ok_or(VmError::InvariantViolation("vm intrinsics not initialized"))?;
  Err(vm_js::throw_type_error(
    scope,
    intrinsics,
    ILLEGAL_CONSTRUCTOR_ERROR,
  ))
}

fn urlsp_construct_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let (limits, fallback_proto) =
    with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
      Ok((state.limits.clone(), state.params_proto))
    })?;

  // WebIDL constructor behavior: `new_target.prototype` is consulted to select the wrapper's
  // prototype, so subclasses like `class Y extends URLSearchParams {}` produce `Y` instances.
  //
  // Note: compute this outside `with_realm_state_mut` (which holds the URL registry lock), because
  // property access can execute user code.
  let proto = prototype_from_new_target(vm, scope, host, hooks, new_target, fallback_proto)?;
  scope.push_root(Value::Object(proto))?;

  let init_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let params = match init_value {
    Value::Undefined => UrlSearchParams::new(&limits),
    Value::Object(obj) => {
      // URLSearchParams(init) accepts:
      // - sequence<sequence<USVString>>
      // - record<USVString, USVString>
      // - USVString
      //
      // For now we interpret objects with an @@iterator method (or array exotic objects) as the
      // sequence form. Otherwise, we treat them as record-like.
      let intrinsics = vm
        .intrinsics()
        .ok_or(VmError::InvariantViolation("vm intrinsics not initialized"))?;
      let iterator_key = PropertyKey::from_symbol(intrinsics.well_known_symbols().iterator);

      match vm.get_method_with_host_and_hooks(host, scope, hooks, Value::Object(obj), iterator_key)
      {
        Ok(Some(method)) => {
          let record =
            iterator::get_iterator_from_method(vm, host, hooks, scope, Value::Object(obj), method)?;
          urlsp_init_from_iterable(vm, scope, host, hooks, &limits, record)?
        }
        Ok(None) => {
          // Array fast-path: `vm-js` supports iterating array exotic objects before full
          // `%Array.prototype%[@@iterator]` exists. If it's not an array, fall back to record.
          match iterator::get_iterator(vm, host, hooks, scope, Value::Object(obj)) {
            Ok(record) => urlsp_init_from_iterable(vm, scope, host, hooks, &limits, record)?,
            Err(_) => urlsp_init_from_record(vm, scope, host, hooks, &limits, obj)?,
          }
        }
        Err(err) => return Err(err),
      }
    }
    other => {
      let init = value_to_limited_string(
        vm,
        scope,
        host,
        hooks,
        other,
        limits.max_input_bytes,
        URLSP_INIT_TOO_LONG_ERROR,
      )?;
      UrlSearchParams::parse(&init, &limits).map_err(map_url_error)?
    }
  };

  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let obj = scope.alloc_object()?;
    scope.heap_mut().object_set_prototype(obj, Some(proto))?;
    scope.heap_mut().object_set_host_slots(
      obj,
      HostSlots {
        a: URL_SEARCH_PARAMS_HOST_TAG,
        b: 0,
      },
    )?;
    state.params.insert(WeakGcObject::from(obj), params);
    Ok(Value::Object(obj))
  })
}

fn urlsp_append_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (params, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let params = require_params(scope, state, this)?;
    Ok((params, state.limits.max_input_bytes))
  })?;

  let name = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URLSP_ARG_TOO_LONG_ERROR,
  )?;
  let value = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(1).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URLSP_ARG_TOO_LONG_ERROR,
  )?;
  params.append(&name, &value).map_err(map_url_error)?;
  Ok(Value::Undefined)
}

fn urlsp_delete_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (params, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let params = require_params(scope, state, this)?;
    Ok((params, state.limits.max_input_bytes))
  })?;

  let name = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URLSP_ARG_TOO_LONG_ERROR,
  )?;
  let value = match args.get(1).copied() {
    None | Some(Value::Undefined) => None,
    Some(v) => Some(value_to_limited_string(
      vm,
      scope,
      host,
      hooks,
      v,
      max_bytes,
      URLSP_ARG_TOO_LONG_ERROR,
    )?),
  };
  params
    .delete(&name, value.as_deref())
    .map_err(map_url_error)?;
  Ok(Value::Undefined)
}

fn urlsp_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (params, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let params = require_params(scope, state, this)?;
    Ok((params, state.limits.max_input_bytes))
  })?;

  let name = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URLSP_ARG_TOO_LONG_ERROR,
  )?;
  match params.get(&name).map_err(map_url_error)? {
    None => Ok(Value::Null),
    Some(v) => {
      let s = scope.alloc_string(&v)?;
      Ok(Value::String(s))
    }
  }
}

fn urlsp_get_all_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intrinsics = vm
    .intrinsics()
    .ok_or(VmError::InvariantViolation("vm intrinsics not initialized"))?;
  let (params, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let params = require_params(scope, state, this)?;
    Ok((params, state.limits.max_input_bytes))
  })?;

  let name = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URLSP_ARG_TOO_LONG_ERROR,
  )?;
  let values = params.get_all(&name).map_err(map_url_error)?;
  let arr = scope.alloc_array(values.len())?;
  scope
    .heap_mut()
    .object_set_prototype(arr, Some(intrinsics.array_prototype()))?;
  // Root the array while populating it: `alloc_key` / `alloc_string` can trigger GC under small
  // `HeapLimits::gc_threshold`, and an unrooted array can be collected and turn `arr` into an
  // invalid handle before we define any properties on it.
  scope.push_root(Value::Object(arr))?;
  for (idx, value) in values.into_iter().enumerate() {
    let idx_u32: u32 = idx
      .try_into()
      .map_err(|_| VmError::Unimplemented("array too large"))?;
    let key = alloc_key(scope, &idx_u32.to_string())?;
    let s = scope.alloc_string(&value)?;
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
  Ok(Value::Object(arr))
}

fn urlsp_has_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (params, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let params = require_params(scope, state, this)?;
    Ok((params, state.limits.max_input_bytes))
  })?;

  let name = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URLSP_ARG_TOO_LONG_ERROR,
  )?;
  let value = match args.get(1).copied() {
    None | Some(Value::Undefined) => None,
    Some(v) => Some(value_to_limited_string(
      vm,
      scope,
      host,
      hooks,
      v,
      max_bytes,
      URLSP_ARG_TOO_LONG_ERROR,
    )?),
  };
  let has = params.has(&name, value.as_deref()).map_err(map_url_error)?;
  Ok(Value::Bool(has))
}

fn urlsp_for_each_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  let this_arg = args.get(1).copied().unwrap_or(Value::Undefined);

  if !scope.heap().is_callable(callback).unwrap_or(false) {
    let intrinsics = vm
      .intrinsics()
      .ok_or(VmError::InvariantViolation("vm intrinsics not initialized"))?;
    return Err(vm_js::throw_type_error(
      scope,
      intrinsics,
      "URLSearchParams.forEach callback is not callable",
    ));
  }

  let params = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    require_params(scope, state, this)
  })?;

  let Value::Object(params_obj) = this else {
    return Err(VmError::TypeError("URLSearchParams: illegal invocation"));
  };

  let mut index = 0usize;
  loop {
    let pairs = params.pairs().map_err(map_url_error)?;
    if index >= pairs.len() {
      break;
    }
    let (name, value) = pairs.get(index).cloned().ok_or(VmError::InvariantViolation(
      "URLSearchParams forEach index out of bounds",
    ))?;
    index = index.saturating_add(1);

    let value_s = scope.alloc_string(&value)?;
    scope.push_root(Value::String(value_s))?;
    let name_s = scope.alloc_string(&name)?;
    scope.push_root(Value::String(name_s))?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      hooks,
      callback,
      this_arg,
      &[
        Value::String(value_s),
        Value::String(name_s),
        Value::Object(params_obj),
      ],
    )?;
  }

  Ok(Value::Undefined)
}

fn urlsp_iter_proto_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.get(1).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "URLSearchParams binding missing iterator prototype native slot",
    )),
  }
}

fn urlsp_entries_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let iter_proto = urlsp_iter_proto_from_callee(scope, callee)?;
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let params = require_params(scope, state, this)?;

    let obj = scope.alloc_object_with_prototype(Some(iter_proto))?;
    scope.heap_mut().object_set_host_slots(
      obj,
      HostSlots {
        a: URL_SEARCH_PARAMS_ITERATOR_HOST_TAG,
        b: 0,
      },
    )?;
    scope.push_root(Value::Object(obj))?;
    state.params_iterators.insert(
      WeakGcObject::from(obj),
      UrlSearchParamsIteratorState {
        params,
        index: 0,
        kind: URLSP_ITER_KIND_ENTRIES,
      },
    );
    Ok(Value::Object(obj))
  })
}

fn urlsp_keys_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let iter_proto = urlsp_iter_proto_from_callee(scope, callee)?;
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let params = require_params(scope, state, this)?;

    let obj = scope.alloc_object_with_prototype(Some(iter_proto))?;
    scope.heap_mut().object_set_host_slots(
      obj,
      HostSlots {
        a: URL_SEARCH_PARAMS_ITERATOR_HOST_TAG,
        b: 0,
      },
    )?;
    scope.push_root(Value::Object(obj))?;
    state.params_iterators.insert(
      WeakGcObject::from(obj),
      UrlSearchParamsIteratorState {
        params,
        index: 0,
        kind: URLSP_ITER_KIND_KEYS,
      },
    );
    Ok(Value::Object(obj))
  })
}

fn urlsp_values_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let iter_proto = urlsp_iter_proto_from_callee(scope, callee)?;
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let params = require_params(scope, state, this)?;

    let obj = scope.alloc_object_with_prototype(Some(iter_proto))?;
    scope.heap_mut().object_set_host_slots(
      obj,
      HostSlots {
        a: URL_SEARCH_PARAMS_ITERATOR_HOST_TAG,
        b: 0,
      },
    )?;
    scope.push_root(Value::Object(obj))?;
    state.params_iterators.insert(
      WeakGcObject::from(obj),
      UrlSearchParamsIteratorState {
        params,
        index: 0,
        kind: URLSP_ITER_KIND_VALUES,
      },
    );
    Ok(Value::Object(obj))
  })
}

fn urlsp_iterator_next_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(iter_obj) = this else {
    return Err(VmError::TypeError(
      "URLSearchParams iterator: illegal invocation",
    ));
  };
  let slots = scope
    .heap()
    .object_host_slots(iter_obj)?
    .ok_or(VmError::TypeError(
      "URLSearchParams iterator: illegal invocation",
    ))?;
  if slots.a != URL_SEARCH_PARAMS_ITERATOR_HOST_TAG {
    return Err(VmError::TypeError(
      "URLSearchParams iterator: illegal invocation",
    ));
  }

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  let result_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(result_obj))?;

  let next: Option<(String, String, u8)> =
    with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
      let iter = state
        .params_iterators
        .get_mut(&WeakGcObject::from(iter_obj))
        .ok_or(VmError::TypeError(
          "URLSearchParams iterator: illegal invocation",
        ))?;

      let pairs = iter.params.pairs().map_err(map_url_error)?;
      if iter.index >= pairs.len() {
        return Ok(None);
      }
      let (name, value) = pairs.get(iter.index).cloned().ok_or(VmError::InvariantViolation(
        "URLSearchParams iterator index out of bounds",
      ))?;
      iter.index = iter.index.saturating_add(1);
      Ok(Some((name, value, iter.kind)))
    })?;

  let value_key = alloc_key(scope, "value")?;
  let done_key = alloc_key(scope, "done")?;
  let data_desc = |value| PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  };

  match next {
    None => {
      scope.define_property(result_obj, value_key, data_desc(Value::Undefined))?;
      scope.define_property(result_obj, done_key, data_desc(Value::Bool(true)))?;
      Ok(Value::Object(result_obj))
    }
    Some((name, value, kind)) => {
      let out_value = match kind {
        URLSP_ITER_KIND_ENTRIES => {
          let arr = scope.alloc_array(2)?;
          scope
            .heap_mut()
            .object_set_prototype(arr, Some(intr.array_prototype()))?;
          scope.push_root(Value::Object(arr))?;

          let name_s = scope.alloc_string(&name)?;
          scope.push_root(Value::String(name_s))?;
          let value_s = scope.alloc_string(&value)?;
          scope.push_root(Value::String(value_s))?;
          let k0 = alloc_key(scope, "0")?;
          let k1 = alloc_key(scope, "1")?;
          scope.define_property(arr, k0, data_desc(Value::String(name_s)))?;
          scope.define_property(arr, k1, data_desc(Value::String(value_s)))?;
          Value::Object(arr)
        }
        URLSP_ITER_KIND_KEYS => {
          let name_s = scope.alloc_string(&name)?;
          Value::String(name_s)
        }
        URLSP_ITER_KIND_VALUES => {
          let value_s = scope.alloc_string(&value)?;
          Value::String(value_s)
        }
        _ => {
          return Err(VmError::TypeError(
            "URLSearchParams iterator: illegal invocation",
          ))
        }
      };

      scope.define_property(result_obj, value_key, data_desc(out_value))?;
      scope.define_property(result_obj, done_key, data_desc(Value::Bool(false)))?;
      Ok(Value::Object(result_obj))
    }
  }
}

fn urlsp_iterator_iterator_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(iter_obj) = this else {
    return Err(VmError::TypeError(
      "URLSearchParams iterator: illegal invocation",
    ));
  };
  let slots = scope
    .heap()
    .object_host_slots(iter_obj)?
    .ok_or(VmError::TypeError(
      "URLSearchParams iterator: illegal invocation",
    ))?;
  if slots.a != URL_SEARCH_PARAMS_ITERATOR_HOST_TAG {
    return Err(VmError::TypeError(
      "URLSearchParams iterator: illegal invocation",
    ));
  }
  Ok(this)
}

fn urlsp_sort_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let params = require_params(scope, state, this)?;
    params.sort().map_err(map_url_error)?;
    Ok(Value::Undefined)
  })
}

fn urlsp_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (params, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let params = require_params(scope, state, this)?;
    Ok((params, state.limits.max_input_bytes))
  })?;

  let name = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URLSP_ARG_TOO_LONG_ERROR,
  )?;
  let value = value_to_limited_string(
    vm,
    scope,
    host,
    hooks,
    args.get(1).copied().unwrap_or(Value::Undefined),
    max_bytes,
    URLSP_ARG_TOO_LONG_ERROR,
  )?;
  params.set(&name, &value).map_err(map_url_error)?;
  Ok(Value::Undefined)
}

fn urlsp_size_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let params = require_params(scope, state, this)?;
    let size = params.size().map_err(map_url_error)?;
    Ok(Value::Number(size as f64))
  })
}

fn urlsp_to_string_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let params = require_params(scope, state, this)?;
    let s = params.serialize().map_err(map_url_error)?;
    let out = scope.alloc_string(&s)?;
    Ok(Value::String(out))
  })
}

/// Installs `URL` and `URLSearchParams` on the realm global object.
pub fn install_window_url_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
) -> Result<(), VmError> {
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let realm_id = realm.id();
  let realm_id_raw = realm_id.to_raw();
  let realm_id_num = realm_id_raw as f64;
  if realm_id_num as u64 != realm_id_raw {
    return Err(VmError::InvariantViolation(
      "realm id is too large to store in URL native slots",
    ));
  }
  let realm_slot = Value::Number(realm_id_num);

  // Fast path: idempotent install (avoid leaking per-realm roots if called twice).
  {
    let registry = registry().lock().unwrap_or_else(|err| err.into_inner());
    if registry.realms.contains_key(&realm_id) {
      return Ok(());
    }
  }

  // --- Prototypes ---
  let url_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(url_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(url_proto, Some(realm.intrinsics().object_prototype()))?;

  let params_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(params_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(params_proto, Some(realm.intrinsics().object_prototype()))?;

  // @@toStringTag branding for platform object detection (`Object.prototype.toString.call(x)`).
  let to_string_tag = realm.intrinsics().well_known_symbols().to_string_tag;
  let url_tag = scope.alloc_string("URL")?;
  scope.push_root(Value::String(url_tag))?;
  scope.define_property(
    url_proto,
    PropertyKey::from_symbol(to_string_tag),
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::String(url_tag),
        writable: false,
      },
    },
  )?;
  let params_tag = scope.alloc_string("URLSearchParams")?;
  scope.push_root(Value::String(params_tag))?;
  scope.define_property(
    params_proto,
    PropertyKey::from_symbol(to_string_tag),
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::String(params_tag),
        writable: false,
      },
    },
  )?;

  // --- Constructors ---
  let url_call_id = vm.register_native_call(url_call_without_new_native)?;
  let url_construct_id = vm.register_native_construct(url_construct_native)?;
  let url_name = scope.alloc_string("URL")?;
  scope.push_root(Value::String(url_name))?;
  let slots = [realm_slot];
  let url_ctor = scope.alloc_native_function_with_slots(
    url_call_id,
    Some(url_construct_id),
    url_name,
    1,
    &slots,
  )?;
  scope
    .heap_mut()
    .object_set_prototype(url_ctor, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(url_ctor))?;

  let sp_call_id = vm.register_native_call(urlsp_call_without_new_native)?;
  let sp_construct_id = vm.register_native_construct(urlsp_construct_native)?;
  let sp_name = scope.alloc_string("URLSearchParams")?;
  scope.push_root(Value::String(sp_name))?;
  let sp_ctor = scope.alloc_native_function_with_slots(
    sp_call_id,
    Some(sp_construct_id),
    sp_name,
    0,
    &slots,
  )?;
  scope
    .heap_mut()
    .object_set_prototype(sp_ctor, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(sp_ctor))?;

  // Expose globals.
  let url_key = alloc_key(&mut scope, "URL")?;
  scope.define_property(global, url_key, data_desc(Value::Object(url_ctor)))?;
  // Legacy alias used by older scripts for `createObjectURL` (non-standard but common in the wild).
  let webkit_url_key = alloc_key(&mut scope, "webkitURL")?;
  scope.define_property(
    global,
    webkit_url_key,
    data_desc(Value::Object(url_ctor)),
  )?;
  let sp_key = alloc_key(&mut scope, "URLSearchParams")?;
  scope.define_property(global, sp_key, data_desc(Value::Object(sp_ctor)))?;

  // Wire prototypes.
  let proto_key = alloc_key(&mut scope, "prototype")?;
  scope.define_property(
    url_ctor,
    proto_key,
    ctor_link_desc(Value::Object(url_proto)),
  )?;
  let constructor_key = alloc_key(&mut scope, "constructor")?;
  scope.define_property(
    url_proto,
    constructor_key,
    ctor_link_desc(Value::Object(url_ctor)),
  )?;

  // --- URL static methods ---
  install_method(
    vm,
    &mut scope,
    realm,
    url_ctor,
    "canParse",
    url_can_parse_native,
    1,
    realm_slot,
  )?;
  install_method(
    vm,
    &mut scope,
    realm,
    url_ctor,
    "parse",
    url_parse_native,
    1,
    realm_slot,
  )?;
  install_method(
    vm,
    &mut scope,
    realm,
    url_ctor,
    "createObjectURL",
    url_create_object_url_native,
    1,
    realm_slot,
  )?;
  install_method(
    vm,
    &mut scope,
    realm,
    url_ctor,
    "revokeObjectURL",
    url_revoke_object_url_native,
    1,
    realm_slot,
  )?;

  let proto_key = alloc_key(&mut scope, "prototype")?;
  scope.define_property(
    sp_ctor,
    proto_key,
    ctor_link_desc(Value::Object(params_proto)),
  )?;
  let constructor_key = alloc_key(&mut scope, "constructor")?;
  scope.define_property(
    params_proto,
    constructor_key,
    ctor_link_desc(Value::Object(sp_ctor)),
  )?;

  // --- URL prototype accessors/methods ---
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "href",
    url_href_get_native,
    Some(url_href_set_native),
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "search",
    url_search_get_native,
    Some(url_search_set_native),
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "hash",
    url_hash_get_native,
    Some(url_hash_set_native),
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "origin",
    url_origin_get_native,
    None,
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "protocol",
    url_protocol_get_native,
    Some(url_protocol_set_native),
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "username",
    url_username_get_native,
    Some(url_username_set_native),
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "password",
    url_password_get_native,
    Some(url_password_set_native),
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "host",
    url_host_get_native,
    Some(url_host_set_native),
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "hostname",
    url_hostname_get_native,
    Some(url_hostname_set_native),
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "port",
    url_port_get_native,
    Some(url_port_set_native),
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "pathname",
    url_pathname_get_native,
    Some(url_pathname_set_native),
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "searchParams",
    url_search_params_get_native,
    None,
    realm_slot,
  )?;

  install_method(
    vm,
    &mut scope,
    realm,
    url_proto,
    "toString",
    url_to_string_native,
    0,
    realm_slot,
  )?;
  install_method(
    vm,
    &mut scope,
    realm,
    url_proto,
    "toJSON",
    url_to_json_native,
    0,
    realm_slot,
  )?;

  // --- URLSearchParams prototype methods ---
  install_method(
    vm,
    &mut scope,
    realm,
    params_proto,
    "append",
    urlsp_append_native,
    2,
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    params_proto,
    "size",
    urlsp_size_get_native,
    None,
    realm_slot,
  )?;
  install_method(
    vm,
    &mut scope,
    realm,
    params_proto,
    "delete",
    urlsp_delete_native,
    1,
    realm_slot,
  )?;
  install_method(
    vm,
    &mut scope,
    realm,
    params_proto,
    "get",
    urlsp_get_native,
    1,
    realm_slot,
  )?;
  install_method(
    vm,
    &mut scope,
    realm,
    params_proto,
    "getAll",
    urlsp_get_all_native,
    1,
    realm_slot,
  )?;
  install_method(
    vm,
    &mut scope,
    realm,
    params_proto,
    "has",
    urlsp_has_native,
    1,
    realm_slot,
  )?;
  install_method(
    vm,
    &mut scope,
    realm,
    params_proto,
    "forEach",
    urlsp_for_each_native,
    1,
    realm_slot,
  )?;

  // Deterministic iteration for URLSearchParams (`entries`/`keys`/`values` + @@iterator).
  let urlsp_iter_proto = {
    let object_proto = realm.intrinsics().object_prototype();
    let func_proto = realm.intrinsics().function_prototype();
    let iterator_sym = realm.intrinsics().well_known_symbols().iterator;
    let iter_proto = scope.alloc_object_with_prototype(Some(object_proto))?;
    scope.push_root(Value::Object(iter_proto))?;

    // @@toStringTag branding for `Object.prototype.toString.call(params.entries())`.
    let iter_tag = scope.alloc_string("URLSearchParams Iterator")?;
    scope.push_root(Value::String(iter_tag))?;
    scope.define_property(
      iter_proto,
      PropertyKey::from_symbol(to_string_tag),
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::String(iter_tag),
          writable: false,
        },
      },
    )?;

    let next_id = vm.register_native_call(urlsp_iterator_next_native)?;
    let next_name = scope.alloc_string("next")?;
    scope.push_root(Value::String(next_name))?;
    let next_fn =
      scope.alloc_native_function_with_slots(next_id, None, next_name, 0, &[realm_slot])?;
    scope
      .heap_mut()
      .object_set_prototype(next_fn, Some(func_proto))?;
    scope.push_root(Value::Object(next_fn))?;
    let next_key = alloc_key(&mut scope, "next")?;
    scope.define_property(
      iter_proto,
      next_key,
      idl_data_desc(Value::Object(next_fn)),
    )?;

    let iter_id = vm.register_native_call(urlsp_iterator_iterator_native)?;
    let iter_name = scope.alloc_string("[Symbol.iterator]")?;
    scope.push_root(Value::String(iter_name))?;
    let iter_fn =
      scope.alloc_native_function_with_slots(iter_id, None, iter_name, 0, &[realm_slot])?;
    scope.push_root(Value::Object(iter_fn))?;
    scope
      .heap_mut()
      .object_set_prototype(iter_fn, Some(func_proto))?;
    let sym_key = PropertyKey::from_symbol(iterator_sym);
    scope.define_property(iter_proto, sym_key, proto_data_desc(Value::Object(iter_fn)))?;

    iter_proto
  };

  let entries_id = vm.register_native_call(urlsp_entries_native)?;
  let entries_name = scope.alloc_string("entries")?;
  scope.push_root(Value::String(entries_name))?;
  let entries_fn = scope.alloc_native_function_with_slots(
    entries_id,
    None,
    entries_name,
    0,
    &[realm_slot, Value::Object(urlsp_iter_proto)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(entries_fn, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(entries_fn))?;
  let entries_key = alloc_key(&mut scope, "entries")?;
  scope.define_property(
    params_proto,
    entries_key,
    idl_data_desc(Value::Object(entries_fn)),
  )?;

  let keys_id = vm.register_native_call(urlsp_keys_native)?;
  let keys_name = scope.alloc_string("keys")?;
  scope.push_root(Value::String(keys_name))?;
  let keys_fn = scope.alloc_native_function_with_slots(
    keys_id,
    None,
    keys_name,
    0,
    &[realm_slot, Value::Object(urlsp_iter_proto)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(keys_fn, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(keys_fn))?;
  let keys_key = alloc_key(&mut scope, "keys")?;
  scope.define_property(
    params_proto,
    keys_key,
    idl_data_desc(Value::Object(keys_fn)),
  )?;

  let values_id = vm.register_native_call(urlsp_values_native)?;
  let values_name = scope.alloc_string("values")?;
  scope.push_root(Value::String(values_name))?;
  let values_fn = scope.alloc_native_function_with_slots(
    values_id,
    None,
    values_name,
    0,
    &[realm_slot, Value::Object(urlsp_iter_proto)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(values_fn, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(values_fn))?;
  let values_key = alloc_key(&mut scope, "values")?;
  scope.define_property(
    params_proto,
    values_key,
    idl_data_desc(Value::Object(values_fn)),
  )?;

  // [Symbol.iterator] is an alias for entries().
  let sym_key = PropertyKey::from_symbol(realm.intrinsics().well_known_symbols().iterator);
  scope.define_property(
    params_proto,
    sym_key,
    proto_data_desc(Value::Object(entries_fn)),
  )?;

  install_method(
    vm,
    &mut scope,
    realm,
    params_proto,
    "sort",
    urlsp_sort_native,
    0,
    realm_slot,
  )?;
  install_method(
    vm,
    &mut scope,
    realm,
    params_proto,
    "set",
    urlsp_set_native,
    2,
    realm_slot,
  )?;
  install_method(
    vm,
    &mut scope,
    realm,
    params_proto,
    "toString",
    urlsp_to_string_native,
    0,
    realm_slot,
  )?;

  // Register per-realm state.
  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  registry.realms.insert(
    realm_id,
    UrlRealmState {
      limits: UrlLimits::default(),
      url_proto,
      params_proto,
      urls: HashMap::new(),
      params: HashMap::new(),
      params_iterators: HashMap::new(),
      cached_search_params: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
      last_gc_runs_cached_search_params: scope.heap().gc_runs(),
    },
  );

  Ok(())
}

pub fn teardown_window_url_bindings_for_realm(realm_id: RealmId, heap: &mut Heap) {
  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  let Some(state) = registry.realms.remove(&realm_id) else {
    return;
  };
  for entry in state.cached_search_params.values() {
    heap.remove_root(entry.params_root);
  }
}

/// Serialize a `URLSearchParams` wrapper for use by other vm-js bindings (notably `fetch()` body
/// conversion).
///
/// Returns `Ok(None)` when `obj` is not a `URLSearchParams` wrapper in the current realm.
pub(crate) fn serialize_url_search_params_for_fetch(
  vm: &Vm,
  heap: &Heap,
  obj: GcObject,
) -> Result<Option<String>, VmError> {
  let Some(realm_id) = vm.current_realm() else {
    return Ok(None);
  };

  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  let Some(state) = registry.realms.get_mut(&realm_id) else {
    return Ok(None);
  };

  // Opportunistically sweep dead wrappers when GC has run.
  let gc_runs = heap.gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    state.urls.retain(|k, _| k.upgrade(heap).is_some());
    state.params.retain(|k, _| k.upgrade(heap).is_some());
    state
      .params_iterators
      .retain(|k, _| k.upgrade(heap).is_some());
  }

  let params = match state.params.get(&WeakGcObject::from(obj)).cloned() {
    Some(p) => p,
    None => return Ok(None),
  };

  params.serialize().map(Some).map_err(map_url_error)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use vm_js::HeapLimits;

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value, got {value:?}");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  fn get_number(value: Value) -> f64 {
    match value {
      Value::Number(n) => n,
      other => panic!("expected number value, got {other:?}"),
    }
  }

  fn get_bool(value: Value) -> bool {
    match value {
      Value::Bool(b) => b,
      other => panic!("expected bool value, got {other:?}"),
    }
  }

  #[test]
  fn object_prototype_to_string_uses_url_to_string_tag() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let url = realm.exec_script("Object.prototype.toString.call(new URL('https://example.invalid/'))")?;
    assert_eq!(get_string(realm.heap(), url), "[object URL]");

    let params =
      realm.exec_script("Object.prototype.toString.call(new URLSearchParams('a=1&b=2'))")?;
    assert_eq!(get_string(realm.heap(), params), "[object URLSearchParams]");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn object_prototype_to_string_uses_url_search_params_iterator_to_string_tag(
  ) -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let entries = realm.exec_script(
      "Object.prototype.toString.call(new URLSearchParams('a=1').entries())",
    )?;
    assert_eq!(
      get_string(realm.heap(), entries),
      "[object URLSearchParams Iterator]"
    );

    let keys =
      realm.exec_script("Object.prototype.toString.call(new URLSearchParams('a=1').keys())")?;
    assert_eq!(
      get_string(realm.heap(), keys),
      "[object URLSearchParams Iterator]"
    );

    let values =
      realm.exec_script("Object.prototype.toString.call(new URLSearchParams('a=1').values())")?;
    assert_eq!(
      get_string(realm.heap(), values),
      "[object URLSearchParams Iterator]"
    );

    realm.teardown();
    Ok(())
  }

  #[test]
  fn url_constructor_supports_subclassing_via_new_target_prototype() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let instanceof_ok = realm.exec_script(
      "(function(){\
         class X extends URL {}\
         const u = new X('https://example.com/');\
         return (u instanceof X) && (u instanceof URL);\
       })()",
    )?;
    assert_eq!(instanceof_ok, Value::Bool(true));

    let proto_ok = realm.exec_script(
      "(function(){\
         class X extends URL {}\
         const u = new X('https://example.com/');\
         return Object.getPrototypeOf(u) === X.prototype;\
       })()",
    )?;
    assert_eq!(proto_ok, Value::Bool(true));

    let href = realm.exec_script(
      "(function(){\
         class X extends URL {}\
         const u = new X('https://example.com/');\
         return u.href;\
       })()",
    )?;
    assert_eq!(get_string(realm.heap(), href), "https://example.com/");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn url_search_params_constructor_supports_subclassing_via_new_target_prototype(
  ) -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let instanceof_ok = realm.exec_script(
      "(function(){\
         class Y extends URLSearchParams {}\
         const p = new Y('a=1');\
         return (p instanceof Y) && (p instanceof URLSearchParams);\
       })()",
    )?;
    assert_eq!(instanceof_ok, Value::Bool(true));

    let serialized = realm.exec_script(
      "(function(){\
         class Y extends URLSearchParams {}\
         const p = new Y('a=1');\
         return p.toString();\
       })()",
    )?;
    assert_eq!(get_string(realm.heap(), serialized), "a=1");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn webkit_url_is_aliased_to_url_constructor() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let is_fn = realm.exec_script("typeof webkitURL === 'function'")?;
    assert!(get_bool(is_fn));

    let is_same = realm.exec_script("webkitURL === URL")?;
    assert!(get_bool(is_same));

    let has_create_object_url = realm.exec_script("typeof webkitURL.createObjectURL === 'function'")?;
    assert!(get_bool(has_create_object_url));

    realm.teardown();
    Ok(())
  }

  #[test]
  fn webidl_members_are_enumerable() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let href_enum =
      realm.exec_script("Object.getOwnPropertyDescriptor(URL.prototype,'href').enumerable")?;
    assert!(get_bool(href_enum));

    let can_parse_enum = realm.exec_script("Object.getOwnPropertyDescriptor(URL,'canParse').enumerable")?;
    assert!(get_bool(can_parse_enum));

    let append_enum = realm
      .exec_script("Object.getOwnPropertyDescriptor(URLSearchParams.prototype,'append').enumerable")?;
    assert!(get_bool(append_enum));

    let sym_iter_enum = realm.exec_script(
      "Object.getOwnPropertyDescriptor(URLSearchParams.prototype, Symbol.iterator).enumerable",
    )?;
    assert!(!get_bool(sym_iter_enum));

    let iter_next_enum = realm.exec_script(
      "Object.getOwnPropertyDescriptor(Object.getPrototypeOf(new URLSearchParams('a=1').entries()), 'next').enumerable",
    )?;
    assert!(get_bool(iter_next_enum));

    realm.teardown();
    Ok(())
  }

  #[test]
  fn url_bindings_are_isolated_per_realm_and_gc_sweep_is_safe() -> Result<(), VmError> {
    let mut realm1 = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let mut realm2 = WindowRealm::new(WindowRealmConfig::new("https://example.net/"))?;

    let href1 = realm1.exec_script("new URL('/a', 'https://example.com/base').href")?;
    assert_eq!(get_string(realm1.heap(), href1), "https://example.com/a");
    let href2 = realm2.exec_script("new URL('/b', 'https://example.net/base').href")?;
    assert_eq!(get_string(realm2.heap(), href2), "https://example.net/b");

    let v1 = realm1.exec_script("new URLSearchParams('a=1').get('a')")?;
    assert_eq!(get_string(realm1.heap(), v1), "1");
    let v2 = realm2.exec_script("new URLSearchParams('a=2').get('a')")?;
    assert_eq!(get_string(realm2.heap(), v2), "2");

    // Force a GC cycle and then invoke URL bindings again to exercise the opportunistic weak-cache
    // sweep path.
    realm1.heap_mut().collect_garbage();
    realm2.heap_mut().collect_garbage();

    let href1b = realm1.exec_script("new URL('https://example.com/c').toString()")?;
    assert_eq!(get_string(realm1.heap(), href1b), "https://example.com/c");
    let q2b = realm2.exec_script("new URLSearchParams('x=y').toString()")?;
    assert_eq!(get_string(realm2.heap(), q2b), "x=y");

    // Teardown should remove per-realm persistent roots without panicking.
    realm1.teardown();
    realm2.teardown();
    Ok(())
  }

  #[test]
  fn url_search_params_cache_is_not_reflectable() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    // Accessing `searchParams` must not define an observable hidden property (e.g. a private
    // `Symbol(__fastrender_...)`) on the URL instance.
    let ok = realm.exec_script(
      "(function(){\
         const u = new URL('https://example.com/?a=1');\
         u.searchParams;\
         const syms = Object.getOwnPropertySymbols(u);\
         if (syms.length === 0) return true;\
         return !syms.some((s) => {\
           const d = s.description || '';\
           return d.includes('fastrender') || d.includes('search_params');\
         });\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    realm.teardown();
    Ok(())
  }

  #[test]
  fn url_search_params_size_and_sort_are_exposed() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let size = realm.exec_script("new URLSearchParams('a=1&a=2').size")?;
    assert_eq!(get_number(size), 2.0);

    let sorted =
      realm.exec_script("const p = new URLSearchParams('b=1&a=2&a=1'); p.sort(); p.toString()")?;
    assert_eq!(get_string(realm.heap(), sorted), "a=2&a=1&b=1");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn url_search_params_get_all_is_safe_under_gc_pressure() -> Result<(), VmError> {
    // Use a very small GC threshold so the allocations inside `URLSearchParams.getAll()` (array
    // allocation, key strings, value strings, property table growth) trigger frequent GC cycles.
    // This stresses rooting invariants: the array returned from `getAll()` must remain live while
    // being populated.
    let config = WindowRealmConfig::new("https://example.com/")
      .with_heap_limits(HeapLimits::new(8 * 1024 * 1024, 4 * 1024));
    let mut realm = WindowRealm::new(config)?;

    let result =
      realm.exec_script("new URLSearchParams('a=1&a=2&a=3').getAll('a').join(',')")?;
    assert_eq!(get_string(realm.heap(), result), "1,2,3");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn url_revoke_object_url_is_noop_for_large_input() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let result = realm.exec_script(
      "(function(){\
         const u = URL.createObjectURL(new Blob(['hi'], { type: 'text/plain' }));\
         URL.revokeObjectURL(u);\
         URL.revokeObjectURL('x'.repeat(10000));\
         return 'ok';\
       })()",
    )?;
    assert_eq!(get_string(realm.heap(), result), "ok");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn url_search_params_iteration_and_for_each_are_exposed() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let for_each = realm.exec_script(
      "(function(){\
         const p = new URLSearchParams('b=1&a=2&a=1');\
         const out = [];\
         p.forEach((v, k) => { out.push(k + '=' + v); });\
         return out.join('&');\
       })()",
    )?;
    assert_eq!(get_string(realm.heap(), for_each), "b=1&a=2&a=1");

    let entries_next = realm.exec_script(
      "(function(){\
         const p = new URLSearchParams('b=1&a=2');\
         const it = p.entries();\
         const out = [];\
         let r;\
         for (;;) {\
           r = it.next();\
           if (r.done) break;\
           out.push(r.value[0] + '=' + r.value[1]);\
         }\
         return out.join('&');\
       })()",
    )?;
    assert_eq!(get_string(realm.heap(), entries_next), "b=1&a=2");

    let keys_next = realm.exec_script(
      "(function(){\
         const p = new URLSearchParams('b=1&a=2');\
         const it = p.keys();\
         const out = [];\
         let r;\
         for (;;) {\
           r = it.next();\
           if (r.done) break;\
           out.push(r.value);\
         }\
         return out.join(',');\
       })()",
    )?;
    assert_eq!(get_string(realm.heap(), keys_next), "b,a");

    let values_next = realm.exec_script(
      "(function(){\
         const p = new URLSearchParams('b=1&a=2');\
         const it = p.values();\
         const out = [];\
         let r;\
         for (;;) {\
           r = it.next();\
           if (r.done) break;\
           out.push(r.value);\
         }\
         return out.join(',');\
       })()",
    )?;
    assert_eq!(get_string(realm.heap(), values_next), "1,2");

    // @@iterator should alias entries().
    let params_val = realm.exec_script("new URLSearchParams('a=1')")?;
    let Value::Object(params_obj) = params_val else {
      return Err(VmError::InvariantViolation(
        "URLSearchParams constructor must return an object",
      ));
    };

    let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(realm_ref.global_object()))?;
    scope.push_root(Value::Object(params_obj))?;
    let entries_key = alloc_key(&mut scope, "entries")?;
    let entries_fn = vm.get(&mut scope, params_obj, entries_key)?;
    let sym_key = PropertyKey::from_symbol(realm_ref.intrinsics().well_known_symbols().iterator);
    let sym_fn = vm.get(&mut scope, params_obj, sym_key)?;
    assert_eq!(
      entries_fn, sym_fn,
      "expected URLSearchParams @@iterator to alias entries()"
    );

    drop(scope);
    realm.teardown();
    Ok(())
  }

  #[test]
  fn url_search_params_iterator_is_live_for_appends() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let out = realm.exec_script(
      "(function(){\
         const p = new URLSearchParams('a=1');\
         const it = p.entries();\
         p.append('b','2');\
         const out = [];\
         let r;\
         for (;;) {\
           r = it.next();\
           if (r.done) break;\
           out.push(r.value[0] + '=' + r.value[1]);\
         }\
         return out.join('&');\
       })()",
    )?;
    assert_eq!(get_string(realm.heap(), out), "a=1&b=2");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn url_search_params_for_each_is_live_for_appends() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let out = realm.exec_script(
      "(function(){\
         const p = new URLSearchParams('a=1');\
         const out = [];\
         p.forEach((v, k) => {\
           out.push(k + '=' + v);\
           if (k === 'a') p.append('b','2');\
         });\
         return out.join('&');\
       })()",
    )?;
    assert_eq!(get_string(realm.heap(), out), "a=1&b=2");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn url_accessors_and_setters_are_exposed() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let result = realm.exec_script(
      "(function(){\
         const u = new URL('https://example.com/a?b=c#d');\
         const before = [u.protocol, u.username, u.password, u.host, u.hostname, u.port, u.pathname].join('|');\
         u.username = 'user';\
         u.password = 'pass';\
         u.hostname = 'example.net';\
         u.port = '8080';\
         u.pathname = '/p';\
         u.protocol = 'http:';\
         const mid = [u.href, u.host, u.hostname, u.port].join('|');\
         u.host = 'example.org:9090';\
         const after = [u.href, u.host, u.hostname, u.port].join('|');\
         return before + '->' + mid + '->' + after;\
       })()",
    )?;

    assert_eq!(
      get_string(realm.heap(), result),
      "https:|||example.com|example.com||/a\
->http://user:pass@example.net:8080/p?b=c#d|example.net:8080|example.net|8080\
->http://user:pass@example.org:9090/p?b=c#d|example.org:9090|example.org|9090"
    );

    realm.teardown();
    Ok(())
  }

  #[test]
  fn url_ipv6_host_and_hostname_are_bracketed() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let hostname = realm.exec_script("new URL('http://[::1]:8080/path').hostname")?;
    assert_eq!(get_string(realm.heap(), hostname), "[::1]");

    let host = realm.exec_script("new URL('http://[::1]:8080/path').host")?;
    assert_eq!(get_string(realm.heap(), host), "[::1]:8080");

    realm.teardown();
    Ok(())
  }

  #[test]
  fn url_setters_are_noop_on_failure() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let result = realm.exec_script(
      "(function(){\
         const u = new URL('https://example.com:8080/a');\
         try { u.port = 'nope'; } catch (e) { return 'threw-port'; }\
         try { u.protocol = '1nvalid:'; } catch (e) { return 'threw-proto'; }\
         return [u.protocol, u.port, u.href].join('|');\
       })()",
    )?;
    assert_eq!(
      get_string(realm.heap(), result),
      "https:|8080|https://example.com:8080/a"
    );

    realm.teardown();
    Ok(())
  }
}
