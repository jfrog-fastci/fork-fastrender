//! WHATWG `URL` + `URLSearchParams` bindings for the `vm-js` script runtime (`WindowRealm`).
//!
//! `vm-js` native handlers currently do not have access to embedder host state during script
//! evaluation (`Vm::call_with_host` supplies a dummy host). This module therefore stores per-object
//! Rust state in a thread-local registry keyed by the active vm-js `RealmId` plus the wrapper
//! object's `WeakGcObject` handle.
//!
//! The registry is swept opportunistically whenever the heap's GC run counter changes.

use crate::js::{Url, UrlError, UrlLimits, UrlSearchParams};
use std::cell::RefCell;
use std::char::decode_utf16;
use std::collections::HashMap;
use vm_js::{
  GcObject, GcString, GcSymbol, Heap, PropertyDescriptor, PropertyKey, PropertyKind, Realm, RealmId,
  RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};

const URL_CALL_WITHOUT_NEW_ERROR: &str = "URL constructor must be called with new";
const URLSP_CALL_WITHOUT_NEW_ERROR: &str = "URLSearchParams constructor must be called with new";
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
  last_gc_runs: u64,
}

thread_local! {
  static REGISTRY: RefCell<UrlRegistry> = RefCell::new(UrlRegistry::default());
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
  f: impl FnOnce(&mut UrlRealmState, &mut Scope<'_>) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let realm_id = realm_id_for_binding_call(vm, scope, callee)?;

  REGISTRY.with(|registry| {
    let mut registry = registry.borrow_mut();
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
    }

    f(state, scope)
  })
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
  scope: &mut Scope<'_>,
  state: &UrlRealmState,
  value: Value,
  max_bytes: usize,
  err: &'static str,
) -> Result<String, VmError> {
  let s: GcString = match value {
    Value::String(s) => s,
    Value::Object(obj) => {
      // `vm-js` does not yet implement full object-to-string coercion for host objects. Special-case
      // URL wrappers so `new URL(rel, baseUrlObj)` behaves like browsers.
      if let Some(url) = state.urls.get(&WeakGcObject::from(obj)) {
        let href = url.href().map_err(map_url_error)?;
        let s = scope.alloc_string(&href)?;
        scope.push_root(Value::String(s))?;
        s
      } else {
        // Fall back to the VM's minimal `ToString`. This currently rejects arbitrary objects until a
        // full `ToPrimitive` implementation lands upstream.
        scope.heap_mut().to_string(value)?
      }
    }
    other => scope.heap_mut().to_string(other)?,
  };

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
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(URL_CALL_WITHOUT_NEW_ERROR))
}

fn url_construct_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let input_value = args.get(0).copied().unwrap_or(Value::Undefined);
    let input = value_to_limited_string(
      scope,
      state,
      input_value,
      state.limits.max_input_bytes,
      URL_INPUT_TOO_LONG_ERROR,
    )?;

    let base = match args.get(1).copied() {
      None | Some(Value::Undefined) => None,
      Some(v) => Some(value_to_limited_string(
        scope,
        state,
        v,
        state.limits.max_input_bytes,
        URL_BASE_TOO_LONG_ERROR,
      )?),
    };

    let url = Url::parse_without_diagnostics(&input, base.as_deref(), &state.limits)
      .map_err(map_url_error)?;

    let obj = scope.alloc_object()?;
    scope
      .heap_mut()
      .object_set_prototype(obj, Some(state.url_proto))?;
    state.urls.insert(WeakGcObject::from(obj), url);
    Ok(Value::Object(obj))
  })
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
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let url = require_url(state, this)?;
    let href = url.href().map_err(map_url_error)?;
    let s = scope.alloc_string(&href)?;
    Ok(Value::String(s))
  })
}

fn url_href_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let url = require_url(state, this)?;
    let value = value_to_limited_string(
      scope,
      state,
      args.get(0).copied().unwrap_or(Value::Undefined),
      state.limits.max_input_bytes,
      URL_INPUT_TOO_LONG_ERROR,
    )?;
    url.set_href(&value).map_err(map_url_error)?;
    Ok(Value::Undefined)
  })
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
  with_realm_state_mut(vm, scope, callee, |state, scope| {
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
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let url = require_url(state, this)?;
    let protocol = url.protocol().map_err(map_url_error)?;
    let s = scope.alloc_string(&protocol)?;
    Ok(Value::String(s))
  })
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
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let url = require_url(state, this)?;
    let host = url.host().map_err(map_url_error)?;
    let s = scope.alloc_string(&host)?;
    Ok(Value::String(s))
  })
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
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let url = require_url(state, this)?;
    let hostname = url.hostname().map_err(map_url_error)?;
    let s = scope.alloc_string(&hostname)?;
    Ok(Value::String(s))
  })
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
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let url = require_url(state, this)?;
    let port = url.port().map_err(map_url_error)?;
    let s = scope.alloc_string(&port)?;
    Ok(Value::String(s))
  })
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
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let url = require_url(state, this)?;
    let pathname = url.pathname().map_err(map_url_error)?;
    let s = scope.alloc_string(&pathname)?;
    Ok(Value::String(s))
  })
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
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let url = require_url(state, this)?;
    let search = url.search().map_err(map_url_error)?;
    let s = scope.alloc_string(&search)?;
    Ok(Value::String(s))
  })
}

fn url_search_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let url = require_url(state, this)?;
    let value = value_to_limited_string(
      scope,
      state,
      args.get(0).copied().unwrap_or(Value::Undefined),
      state.limits.max_input_bytes,
      URL_INPUT_TOO_LONG_ERROR,
    )?;
    ignore_setter_failure(url.set_search(&value))?;
    Ok(Value::Undefined)
  })
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
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let url = require_url(state, this)?;
    let hash = url.hash().map_err(map_url_error)?;
    let s = scope.alloc_string(&hash)?;
    Ok(Value::String(s))
  })
}

fn url_hash_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let url = require_url(state, this)?;
    let value = value_to_limited_string(
      scope,
      state,
      args.get(0).copied().unwrap_or(Value::Undefined),
      state.limits.max_input_bytes,
      URL_INPUT_TOO_LONG_ERROR,
    )?;
    ignore_setter_failure(url.set_hash(&value))?;
    Ok(Value::Undefined)
  })
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
  with_realm_state_mut(vm, scope, callee, |state, scope| {
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
  with_realm_state_mut(vm, scope, callee, |state, scope| {
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
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(URLSP_CALL_WITHOUT_NEW_ERROR))
}

fn urlsp_construct_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let init_value = args.get(0).copied().unwrap_or(Value::Undefined);
    let init = match init_value {
      Value::Undefined => None,
      v => Some(value_to_limited_string(
        scope,
        state,
        v,
        state.limits.max_input_bytes,
        URLSP_INIT_TOO_LONG_ERROR,
      )?),
    };

    let params = match init {
      None => UrlSearchParams::new(&state.limits),
      Some(s) => UrlSearchParams::parse(&s, &state.limits).map_err(map_url_error)?,
    };

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
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let params = require_params(state, this)?;
    let name = value_to_limited_string(
      scope,
      state,
      args.get(0).copied().unwrap_or(Value::Undefined),
      state.limits.max_input_bytes,
      URLSP_ARG_TOO_LONG_ERROR,
    )?;
    let value = value_to_limited_string(
      scope,
      state,
      args.get(1).copied().unwrap_or(Value::Undefined),
      state.limits.max_input_bytes,
      URLSP_ARG_TOO_LONG_ERROR,
    )?;
    params.append(&name, &value).map_err(map_url_error)?;
    Ok(Value::Undefined)
  })
}

fn urlsp_delete_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let params = require_params(state, this)?;
    let name = value_to_limited_string(
      scope,
      state,
      args.get(0).copied().unwrap_or(Value::Undefined),
      state.limits.max_input_bytes,
      URLSP_ARG_TOO_LONG_ERROR,
    )?;
    let value = match args.get(1).copied() {
      None | Some(Value::Undefined) => None,
      Some(v) => Some(value_to_limited_string(
        scope,
        state,
        v,
        state.limits.max_input_bytes,
        URLSP_ARG_TOO_LONG_ERROR,
      )?),
    };
    params
      .delete(&name, value.as_deref())
      .map_err(map_url_error)?;
    Ok(Value::Undefined)
  })
}

fn urlsp_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let params = require_params(state, this)?;
    let name = value_to_limited_string(
      scope,
      state,
      args.get(0).copied().unwrap_or(Value::Undefined),
      state.limits.max_input_bytes,
      URLSP_ARG_TOO_LONG_ERROR,
    )?;
    match params.get(&name).map_err(map_url_error)? {
      None => Ok(Value::Null),
      Some(v) => {
        let s = scope.alloc_string(&v)?;
        Ok(Value::String(s))
      }
    }
  })
}

fn urlsp_get_all_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intrinsics = vm
    .intrinsics()
    .ok_or(VmError::InvariantViolation("vm intrinsics not initialized"))?;
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let params = require_params(state, this)?;
    let name = value_to_limited_string(
      scope,
      state,
      args.get(0).copied().unwrap_or(Value::Undefined),
      state.limits.max_input_bytes,
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
  })
}

fn urlsp_has_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let params = require_params(state, this)?;
    let name = value_to_limited_string(
      scope,
      state,
      args.get(0).copied().unwrap_or(Value::Undefined),
      state.limits.max_input_bytes,
      URLSP_ARG_TOO_LONG_ERROR,
    )?;
    let value = match args.get(1).copied() {
      None | Some(Value::Undefined) => None,
      Some(v) => Some(value_to_limited_string(
        scope,
        state,
        v,
        state.limits.max_input_bytes,
        URLSP_ARG_TOO_LONG_ERROR,
      )?),
    };
    let has = params.has(&name, value.as_deref()).map_err(map_url_error)?;
    Ok(Value::Bool(has))
  })
}

fn urlsp_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, scope| {
    let params = require_params(state, this)?;
    let name = value_to_limited_string(
      scope,
      state,
      args.get(0).copied().unwrap_or(Value::Undefined),
      state.limits.max_input_bytes,
      URLSP_ARG_TOO_LONG_ERROR,
    )?;
    let value = value_to_limited_string(
      scope,
      state,
      args.get(1).copied().unwrap_or(Value::Undefined),
      state.limits.max_input_bytes,
      URLSP_ARG_TOO_LONG_ERROR,
    )?;
    params.set(&name, &value).map_err(map_url_error)?;
    Ok(Value::Undefined)
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
  with_realm_state_mut(vm, scope, callee, |state, scope| {
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
  if REGISTRY.with(|registry| registry.borrow().realms.contains_key(&realm_id)) {
    return Ok(());
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
    scope.alloc_native_function_with_slots(sp_call_id, Some(sp_construct_id), sp_name, 1, &slots)?;
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
    None,
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "host",
    url_host_get_native,
    None,
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "hostname",
    url_hostname_get_native,
    None,
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "port",
    url_port_get_native,
    None,
    realm_slot,
  )?;
  install_accessor(
    vm,
    &mut scope,
    realm,
    url_proto,
    "pathname",
    url_pathname_get_native,
    None,
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
  REGISTRY.with(|registry| {
    let mut registry = registry.borrow_mut();
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
        last_gc_runs: scope.heap().gc_runs(),
      },
    );
  });

  Ok(())
}

pub fn teardown_window_url_bindings_for_realm(realm_id: RealmId, heap: &mut Heap) {
  REGISTRY.with(|registry| {
    let mut registry = registry.borrow_mut();
    let Some(state) = registry.realms.remove(&realm_id) else {
      return;
    };
    heap.remove_root(state.search_params_slot_root);
  });
}
