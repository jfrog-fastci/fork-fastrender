//! WHATWG `URL` + `URLSearchParams` bindings for the `vm-js` script runtime (`WindowRealm`).
//!
//! `URL`/`URLSearchParams` wrappers need per-object Rust state (the parsed URL + query list) and
//! `vm-js` native call hooks do not currently provide a convenient per-realm host state slot.
//! Instead, the bindings store wrapper state in a process-global weak registry keyed by the
//! `RealmId` plus the wrapper object's `WeakGcObject` handle.
//!
//! The registry is swept opportunistically whenever the heap's GC run counter changes.

use crate::js::{Url, UrlError, UrlLimits, UrlSearchParams};
use std::char::decode_utf16;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use vm_js::{
  GcObject, GcString, GcSymbol, Heap, PropertyDescriptor, PropertyKey, PropertyKind, Realm, RealmId,
  RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};
use vm_js::iterator;

const ILLEGAL_CONSTRUCTOR_ERROR: &str = "Illegal constructor";
const URL_INVALID_ERROR: &str = "Invalid URL";
const URL_INPUT_TOO_LONG_ERROR: &str = "URL constructor input exceeded max bytes";
const URL_BASE_TOO_LONG_ERROR: &str = "URL constructor base exceeded max bytes";
const URLSP_INIT_TOO_LONG_ERROR: &str = "URLSearchParams constructor init exceeded max bytes";
const URLSP_ARG_TOO_LONG_ERROR: &str = "URLSearchParams argument exceeded max bytes";

/// Symbol description for the hidden `URL` → cached `URLSearchParams` object slot.
///
/// The actual symbol is allocated with this description but is not exposed anywhere, so it remains
/// effectively private to the realm.
const SEARCH_PARAMS_SLOT_DESC: &str = "__fastrender_url_search_params_slot";

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

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

const URLSP_ITER_KIND_ENTRIES: u8 = 0;
const URLSP_ITER_KIND_KEYS: u8 = 1;
const URLSP_ITER_KIND_VALUES: u8 = 2;

struct UrlSearchParamsIteratorState {
  pairs: Vec<(String, String)>,
  index: usize,
  kind: u8,
}

#[derive(Default)]
struct UrlRegistry {
  realms: HashMap<RealmId, UrlRealmState>,
}

struct UrlRealmState {
  limits: UrlLimits,
  url_proto: GcObject,
  params_proto: GcObject,
  search_params_slot_sym: GcSymbol,
  search_params_slot_root: RootId,
  urls: HashMap<WeakGcObject, Url>,
  params: HashMap<WeakGcObject, UrlSearchParams>,
  params_iterators: HashMap<WeakGcObject, UrlSearchParamsIteratorState>,
  last_gc_runs: u64,
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
  let realm_id = slots
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

  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
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
    let heap = scope.heap();
    state.urls.retain(|k, _| k.upgrade(heap).is_some());
    state.params.retain(|k, _| k.upgrade(heap).is_some());
    state.params_iterators.retain(|k, _| k.upgrade(heap).is_some());
  }

  f(vm, state, scope)
}

fn require_url(state: &UrlRealmState, this: Value) -> Result<Url, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  state
    .urls
    .get(&WeakGcObject::from(obj))
    .cloned()
    .ok_or(VmError::TypeError("Illegal invocation"))
}

fn require_params(state: &UrlRealmState, this: Value) -> Result<UrlSearchParams, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
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
  let mut out = String::with_capacity(capacity);
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
  scope.define_property(proto, key, proto_data_desc(Value::Object(func)))?;
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
  scope.define_property(proto, key, accessor_desc(Value::Object(get_func), set_value))?;
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
  if new_target != Value::Object(callee) {
    let intrinsics = vm
      .intrinsics()
      .ok_or(VmError::InvariantViolation("vm intrinsics not initialized"))?;
    return Err(vm_js::throw_type_error(
      scope,
      intrinsics,
      ILLEGAL_CONSTRUCTOR_ERROR,
    ));
  }

  let limits = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| Ok(state.limits.clone()))?;

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
    scope
      .heap_mut()
      .object_set_prototype(obj, Some(state.url_proto))?;
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
  let limits =
    with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| Ok(state.limits.clone()))?;

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
  let limits =
    with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| Ok(state.limits.clone()))?;

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
    let url = require_url(state, this)?;
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
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let url = require_url(state, this)?;
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
    let url = require_url(state, this)?;
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
    let url = require_url(state, this)?;
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
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let url = require_url(state, this)?;
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
    let url = require_url(state, this)?;
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
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let url = require_url(state, this)?;
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
    let _ = iterator::iterator_close(vm, host, hooks, scope, &record);
    return Err(vm_js::throw_type_error(
      scope,
      intrinsics,
      "URLSearchParams init pair must contain exactly two values",
    ));
  };
  let Some(value_value) = iterator::iterator_step_value(vm, host, hooks, scope, &mut record)? else {
    let _ = iterator::iterator_close(vm, host, hooks, scope, &record);
    return Err(vm_js::throw_type_error(
      scope,
      intrinsics,
      "URLSearchParams init pair must contain exactly two values",
    ));
  };
  if iterator::iterator_step_value(vm, host, hooks, scope, &mut record)?.is_some() {
    let _ = iterator::iterator_close(vm, host, hooks, scope, &record);
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
    while let Some(pair_value) = iterator::iterator_step_value(vm, host, hooks, scope, &mut record)? {
      let (name, value) =
        urlsp_init_pair_from_sequence(vm, scope, host, hooks, limits, pair_value)?;
      params.append(&name, &value).map_err(map_url_error)?;
    }
    Ok(())
  })();

  match result {
    Ok(()) => Ok(params),
    Err(err) => {
      let _ = iterator::iterator_close(vm, host, hooks, scope, &record);
      // If iterator close threw, prefer the original error.
      Err(err)
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

    let name = js_string_to_rust_string_limited(scope, name_key, limits.max_input_bytes, URLSP_INIT_TOO_LONG_ERROR)?;
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
    let url = require_url(state, this)?;
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
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let url = require_url(state, this)?;
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
    let url = require_url(state, this)?;
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
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let url = require_url(state, this)?;
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
    let url = require_url(state, this)?;
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
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let url = require_url(state, this)?;
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
    let url = require_url(state, this)?;
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
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let url = require_url(state, this)?;
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
    let url = require_url(state, this)?;
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
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let url = require_url(state, this)?;
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
    let url = require_url(state, this)?;
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
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let url = require_url(state, this)?;
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
    let url = require_url(state, this)?;
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
  let (url, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let url = require_url(state, this)?;
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
  with_realm_state_mut(vm, scope, callee, |_vm, state, scope| {
    let Value::Object(url_obj) = this else {
      return Err(VmError::TypeError("Illegal invocation"));
    };

    // Brand check.
    let url = state
      .urls
      .get(&WeakGcObject::from(url_obj))
      .cloned()
      .ok_or(VmError::TypeError("Illegal invocation"))?;

    let slot_key = PropertyKey::from_symbol(state.search_params_slot_sym);
    if let Some(existing) = scope
      .heap()
      .object_get_own_data_property_value(url_obj, &slot_key)?
    {
      if !matches!(existing, Value::Undefined) {
        return Ok(existing);
      }
    }

    let params_obj = scope.alloc_object()?;
    scope
      .heap_mut()
      .object_set_prototype(params_obj, Some(state.params_proto))?;
    let params = url.search_params();
    state
      .params
      .insert(WeakGcObject::from(params_obj), params);

    // Cache on the URL object so repeated `url.searchParams` returns the same object and so the
    // params object stays GC-reachable while the URL object is alive.
    scope.define_property(
      url_obj,
      slot_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Object(params_obj),
          writable: false,
        },
      },
    )?;

    Ok(Value::Object(params_obj))
  })
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
    let url = require_url(state, this)?;
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
  if new_target != Value::Object(callee) {
    let intrinsics = vm
      .intrinsics()
      .ok_or(VmError::InvariantViolation("vm intrinsics not initialized"))?;
    return Err(vm_js::throw_type_error(
      scope,
      intrinsics,
      ILLEGAL_CONSTRUCTOR_ERROR,
    ));
  }

  let limits = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| Ok(state.limits.clone()))?;

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

      match vm.get_method_with_host_and_hooks(host, scope, hooks, Value::Object(obj), iterator_key) {
        Ok(Some(method)) => {
          let record = iterator::get_iterator_from_method(
            vm,
            host,
            hooks,
            scope,
            Value::Object(obj),
            method,
          )?;
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
    scope
      .heap_mut()
      .object_set_prototype(obj, Some(state.params_proto))?;
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
  let (params, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let params = require_params(state, this)?;
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
  let (params, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let params = require_params(state, this)?;
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
  let (params, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let params = require_params(state, this)?;
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
  let (params, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let params = require_params(state, this)?;
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
  for (idx, value) in values.into_iter().enumerate() {
    let idx_u32: u32 = idx.try_into().map_err(|_| VmError::Unimplemented("array too large"))?;
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
  let (params, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let params = require_params(state, this)?;
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

  let pairs = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let params = require_params(state, this)?;
    params.pairs().map_err(map_url_error)
  })?;

  let Value::Object(params_obj) = this else {
    return Err(VmError::TypeError("URLSearchParams: illegal invocation"));
  };

  for (name, value) in pairs {
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
      &[Value::String(value_s), Value::String(name_s), Value::Object(params_obj)],
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
    let params = require_params(state, this)?;
    let pairs = params.pairs().map_err(map_url_error)?;

    let obj = scope.alloc_object_with_prototype(Some(iter_proto))?;
    scope.push_root(Value::Object(obj))?;
    state.params_iterators.insert(
      WeakGcObject::from(obj),
      UrlSearchParamsIteratorState {
        pairs,
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
    let params = require_params(state, this)?;
    let pairs = params.pairs().map_err(map_url_error)?;

    let obj = scope.alloc_object_with_prototype(Some(iter_proto))?;
    scope.push_root(Value::Object(obj))?;
    state.params_iterators.insert(
      WeakGcObject::from(obj),
      UrlSearchParamsIteratorState {
        pairs,
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
    let params = require_params(state, this)?;
    let pairs = params.pairs().map_err(map_url_error)?;

    let obj = scope.alloc_object_with_prototype(Some(iter_proto))?;
    scope.push_root(Value::Object(obj))?;
    state.params_iterators.insert(
      WeakGcObject::from(obj),
      UrlSearchParamsIteratorState {
        pairs,
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
    return Err(VmError::TypeError("URLSearchParams iterator: illegal invocation"));
  };

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
        .ok_or(VmError::TypeError("URLSearchParams iterator: illegal invocation"))?;
      if iter.index >= iter.pairs.len() {
        return Ok(None);
      }
      let (name, value) = iter
        .pairs
        .get(iter.index)
        .cloned()
        .ok_or(VmError::InvariantViolation(
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
        _ => return Err(VmError::TypeError("URLSearchParams iterator: illegal invocation")),
      };

      scope.define_property(result_obj, value_key, data_desc(out_value))?;
      scope.define_property(result_obj, done_key, data_desc(Value::Bool(false)))?;
      Ok(Value::Object(result_obj))
    }
  }
}

fn urlsp_iterator_iterator_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
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
  with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let params = require_params(state, this)?;
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
  let (params, max_bytes) = with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let params = require_params(state, this)?;
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
  with_realm_state_mut(vm, scope, callee, |_vm, state, _scope| {
    let params = require_params(state, this)?;
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
    let params = require_params(state, this)?;
    let s = params.serialize().map_err(map_url_error)?;
    let out = scope.alloc_string(&s)?;
    Ok(Value::String(out))
  })
}

/// Installs `URL` and `URLSearchParams` on the realm global object.
pub fn install_window_url_bindings(vm: &mut Vm, realm: &Realm, heap: &mut Heap) -> Result<(), VmError> {
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
    let registry = registry()
      .lock()
      .unwrap_or_else(|err| err.into_inner());
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

  // Hidden slot symbol for caching the `searchParams` object on URL instances.
  let search_params_slot_sym = scope.alloc_symbol(Some(SEARCH_PARAMS_SLOT_DESC))?;
  let search_params_slot_root = scope
    .heap_mut()
    .add_root(Value::Symbol(search_params_slot_sym))?;

  // --- Constructors ---
  let url_call_id = vm.register_native_call(url_call_without_new_native)?;
  let url_construct_id = vm.register_native_construct(url_construct_native)?;
  let url_name = scope.alloc_string("URL")?;
  scope.push_root(Value::String(url_name))?;
  let slots = [realm_slot];
  let url_ctor =
    scope.alloc_native_function_with_slots(url_call_id, Some(url_construct_id), url_name, 1, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(url_ctor, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(url_ctor))?;

  let sp_call_id = vm.register_native_call(urlsp_call_without_new_native)?;
  let sp_construct_id = vm.register_native_construct(urlsp_construct_native)?;
  let sp_name = scope.alloc_string("URLSearchParams")?;
  scope.push_root(Value::String(sp_name))?;
  let sp_ctor =
    scope.alloc_native_function_with_slots(sp_call_id, Some(sp_construct_id), sp_name, 0, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(sp_ctor, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(sp_ctor))?;

  // Expose globals.
  let url_key = alloc_key(&mut scope, "URL")?;
  scope.define_property(global, url_key, data_desc(Value::Object(url_ctor)))?;
  let sp_key = alloc_key(&mut scope, "URLSearchParams")?;
  scope.define_property(global, sp_key, data_desc(Value::Object(sp_ctor)))?;

  // Wire prototypes.
  let proto_key = alloc_key(&mut scope, "prototype")?;
  scope.define_property(url_ctor, proto_key, ctor_link_desc(Value::Object(url_proto)))?;
  let constructor_key = alloc_key(&mut scope, "constructor")?;
  scope.define_property(url_proto, constructor_key, ctor_link_desc(Value::Object(url_ctor)))?;

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

  let proto_key = alloc_key(&mut scope, "prototype")?;
  scope.define_property(sp_ctor, proto_key, ctor_link_desc(Value::Object(params_proto)))?;
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

    let next_id = vm.register_native_call(urlsp_iterator_next_native)?;
    let next_name = scope.alloc_string("next")?;
    scope.push_root(Value::String(next_name))?;
    let next_fn = scope.alloc_native_function_with_slots(next_id, None, next_name, 0, &[realm_slot])?;
    scope
      .heap_mut()
      .object_set_prototype(next_fn, Some(func_proto))?;
    scope.push_root(Value::Object(next_fn))?;
    let next_key = alloc_key(&mut scope, "next")?;
    scope.define_property(iter_proto, next_key, proto_data_desc(Value::Object(next_fn)))?;

    let iter_id = vm.register_native_call(urlsp_iterator_iterator_native)?;
    let iter_name = scope.alloc_string("[Symbol.iterator]")?;
    scope.push_root(Value::String(iter_name))?;
    let iter_fn = scope.alloc_native_function_with_slots(iter_id, None, iter_name, 0, &[realm_slot])?;
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
  scope.define_property(params_proto, entries_key, proto_data_desc(Value::Object(entries_fn)))?;

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
  scope.define_property(params_proto, keys_key, proto_data_desc(Value::Object(keys_fn)))?;

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
  scope.define_property(params_proto, values_key, proto_data_desc(Value::Object(values_fn)))?;

  // [Symbol.iterator] is an alias for entries().
  let sym_key = PropertyKey::from_symbol(realm.intrinsics().well_known_symbols().iterator);
  scope.define_property(params_proto, sym_key, proto_data_desc(Value::Object(entries_fn)))?;

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
  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  registry.realms.insert(
    realm_id,
    UrlRealmState {
      limits: UrlLimits::default(),
      url_proto,
      params_proto,
      search_params_slot_sym,
      search_params_slot_root,
      urls: HashMap::new(),
      params: HashMap::new(),
      params_iterators: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
    },
  );

  Ok(())
}

pub fn teardown_window_url_bindings_for_realm(realm_id: RealmId, heap: &mut Heap) {
  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  let Some(state) = registry.realms.remove(&realm_id) else {
    return;
  };
  heap.remove_root(state.search_params_slot_root);
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

  fn get_number(value: Value) -> f64 {
    match value {
      Value::Number(n) => n,
      other => panic!("expected number value, got {other:?}"),
    }
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
  fn url_search_params_size_and_sort_are_exposed() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let size = realm.exec_script("new URLSearchParams('a=1&a=2').size")?;
    assert_eq!(get_number(size), 2.0);

    let sorted = realm.exec_script(
      "const p = new URLSearchParams('b=1&a=2&a=1'); p.sort(); p.toString()",
    )?;
    assert_eq!(get_string(realm.heap(), sorted), "a=2&a=1&b=1");

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
