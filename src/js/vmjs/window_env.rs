use crate::js::webidl::legacy::VmJsRuntime;
use crate::style::media::{MediaContext, MediaQuery};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use vm_js::{
  promise_resolve_with_host_and_hooks, GcObject, HostSlots, NativeFunctionId, PropertyDescriptor,
  PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};
use webidl_js_runtime::JsRuntime as _;

/// Stable `navigator.userAgent` string reported by FastRender.
///
/// This string is intentionally deterministic and does not depend on the host OS. We keep it
/// aligned with the default HTTP `User-Agent` header so that pages which sniff UA strings (common
/// in scripts gating "unsupported browser" experiences) see a consistent environment.
pub const FASTRENDER_USER_AGENT: &str = crate::resource::DEFAULT_USER_AGENT;

/// Upper bound on the length of media query strings accepted by `matchMedia`.
///
/// This is measured in UTF-16 code units (the JS engine's internal representation).
/// Queries longer than this are treated as invalid (`matches == false`) and the returned
/// `MediaQueryList.media` string is truncated to this length.
const MAX_MATCH_MEDIA_QUERY_CODE_UNITS: usize = 4096;
const MAX_SEND_BEACON_URL_CODE_UNITS: usize = 8192;

// HostSlots tags for platform objects installed by this module.
//
// These are only used for branding: structuredClone must reject them as platform objects.
const NAVIGATOR_HOST_TAG: u64 = 0x4E41_5649_4741_5452; // "NAVIGATR"
const USER_AGENT_DATA_HOST_TAG: u64 = 0x5541_4441_5441_5F5F; // "UADATA__"
const SCREEN_HOST_TAG: u64 = 0x5343_5245_454E_5F5F; // "SCREEN__"
const MEDIA_QUERY_LIST_HOST_TAG: u64 = 0x4D45_4449_4151_5259; // "MEDIAQRY"

const MATCH_MEDIA_SLOT_ENV_ID: usize = 0;
const MATCH_MEDIA_SLOT_MQL_MATCHES_GET_CALL_ID: usize = 1;
const MATCH_MEDIA_SLOT_MQL_ADD_EVENT_LISTENER_CALL_ID: usize = 2;
const MATCH_MEDIA_SLOT_MQL_REMOVE_EVENT_LISTENER_CALL_ID: usize = 3;
const MATCH_MEDIA_SLOT_MQL_ADD_LISTENER_CALL_ID: usize = 4;
const MATCH_MEDIA_SLOT_MQL_REMOVE_LISTENER_CALL_ID: usize = 5;
const MATCH_MEDIA_SLOT_MQL_ONCHANGE_GET_CALL_ID: usize = 6;
const MATCH_MEDIA_SLOT_MQL_ONCHANGE_SET_CALL_ID: usize = 7;

const MAX_TRACKED_MEDIA_QUERY_LISTS_PER_ENV: usize = 1024;
const MAX_MQL_CHANGE_DISPATCHES_PER_ENV_UPDATE: usize = 1024;

const MQL_LISTENERS_KEY: &str = "__fastrender_mql_change_listeners";
const MQL_ONCHANGE_KEY: &str = "__fastrender_mql_onchange";

const MQL_MATCHES_GET_SLOT_ENV_ID: usize = 0;
const MQL_MATCHES_GET_SLOT_TOO_LONG: usize = 1;
const MQL_MATCHES_GET_SLOT_QUERY_STRING: usize = 2;

const UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_MAJOR_VERSION: usize = 0;
const UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_FULL_VERSION: usize = 1;
const UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_PLATFORM: usize = 2;
const UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_MOBILE: usize = 3;
const UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_PLATFORM_VERSION: usize = 4;
const UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_ARCHITECTURE: usize = 5;
const UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_BITNESS: usize = 6;
const UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_MODEL: usize = 7;

const MAX_UA_DATA_STRING_CHARS: usize = 64;
const MAX_UA_DATA_VERSION_CHARS: usize = 32;
const MAX_UA_DATA_HINTS: usize = 32;
const MAX_UA_DATA_HINT_STRING_CODE_UNITS: usize = 64;

fn ua_data_platform_version(platform: &str) -> &'static str {
  match platform {
    // Keep deterministic: do not sniff host OS version.
    "Windows" => "10.0.0",
    _ => "0.0.0",
  }
}

#[derive(Debug, Clone)]
struct UaDataInfo {
  major_version: String,
  full_version: String,
  platform: String,
  mobile: bool,
}

fn clamp_str_chars(s: &str, max_chars: usize) -> String {
  let mut out = String::new();
  if max_chars == 0 {
    return out;
  }
  for (idx, ch) in s.chars().enumerate() {
    if idx >= max_chars {
      break;
    }
    out.push(ch);
  }
  out
}

fn chrome_version_from_user_agent(user_agent: &str) -> Option<&str> {
  let start = user_agent.find("Chrome/")? + "Chrome/".len();
  let tail = user_agent.get(start..)?;
  let end = tail
    .find(|c: char| c.is_whitespace() || matches!(c, ';' | ')')) // typical UA token terminators
    .unwrap_or(tail.len());
  let token = tail.get(..end)?;
  if token.is_empty() {
    None
  } else {
    Some(token)
  }
}

fn ua_data_info_from_env(env: &WindowEnv) -> UaDataInfo {
  let full_version_raw = chrome_version_from_user_agent(env.user_agent).unwrap_or("0.0.0.0");
  let full_version = clamp_str_chars(full_version_raw, MAX_UA_DATA_VERSION_CHARS);
  let major_version_raw = full_version
    .split('.')
    .next()
    .filter(|s| !s.is_empty())
    .unwrap_or("0");
  let major_version = clamp_str_chars(major_version_raw, MAX_UA_DATA_VERSION_CHARS);

  let platform_raw = match env.platform {
    // `navigator.platform` reports `"Win32"` in our default env, but `NavigatorUAData.platform`
    // uses `"Windows"` in Chromium.
    "Win32" => "Windows",
    "MacIntel" => "macOS",
    "Linux" => "Linux",
    other => other,
  };
  let platform = clamp_str_chars(platform_raw, MAX_UA_DATA_STRING_CHARS);

  // Deterministic heuristic derived solely from the provided env.
  let mobile = env.user_agent.contains("Mobile");

  UaDataInfo {
    major_version,
    full_version,
    platform,
    mobile,
  }
}

/// Window-like environment configuration used to install browser shims.
#[derive(Debug, Clone)]
pub struct WindowEnv {
  pub media: MediaContext,
  pub user_agent: &'static str,
  pub platform: &'static str,
  pub language: &'static str,
  pub languages: &'static [&'static str],
}

impl WindowEnv {
  pub fn from_media(media: MediaContext) -> Self {
    Self {
      media,
      user_agent: FASTRENDER_USER_AGENT,
      // Match the UA string above (`DEFAULT_USER_AGENT` uses a Windows Chrome UA).
      platform: "Win32",
      language: "en-US",
      languages: &["en-US", "en"],
    }
  }
}

static NEXT_MATCH_MEDIA_ENV_ID: AtomicU64 = AtomicU64::new(1);
static MATCH_MEDIA_ENVS: OnceLock<Mutex<HashMap<u64, MediaContext>>> = OnceLock::new();
static MATCH_MEDIA_MQLS: OnceLock<Mutex<HashMap<u64, MatchMediaMqlEnvRegistry>>> = OnceLock::new();

#[derive(Debug)]
struct MatchMediaMqlEnvRegistry {
  /// Live `MediaQueryList` objects created by this realm, stored as weak handles so the host does
  /// not keep them alive.
  mqls: Vec<TrackedMediaQueryList>,
  /// Whether a `MediaQueryList` update task has already been queued for this environment.
  update_task_queued: bool,
}

#[derive(Debug)]
struct TrackedMediaQueryList {
  weak: WeakGcObject,
  /// The query string used for parsing/evaluation.
  ///
  /// This is truncated to [`MAX_MATCH_MEDIA_QUERY_CODE_UNITS`] UTF-16 code units when the original
  /// `matchMedia(..)` input exceeds that bound (in which case `too_long == true` and the query is
  /// treated as invalid).
  query_text: String,
  /// Parsed query list for `query_text`, when available.
  queries: Option<Vec<MediaQuery>>,
  too_long: bool,
  last_matches: bool,
}

fn match_media_envs() -> &'static Mutex<HashMap<u64, MediaContext>> {
  MATCH_MEDIA_ENVS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn match_media_mqls() -> &'static Mutex<HashMap<u64, MatchMediaMqlEnvRegistry>> {
  MATCH_MEDIA_MQLS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_match_media_env(media: MediaContext) -> u64 {
  let id = NEXT_MATCH_MEDIA_ENV_ID.fetch_add(1, Ordering::Relaxed);
  match_media_envs().lock().insert(id, media);
  id
}

pub(crate) fn unregister_match_media_env(id: u64) {
  match_media_envs().lock().remove(&id);
  match_media_mqls().lock().remove(&id);
}

fn with_match_media_env<T>(id: u64, f: impl FnOnce(&MediaContext) -> T) -> Option<T> {
  let lock = match_media_envs().lock();
  let env = lock.get(&id)?;
  Some(f(env))
}

pub(crate) fn set_match_media_env_media(id: u64, media: MediaContext) {
  let mut envs = match_media_envs().lock();
  if envs.contains_key(&id) {
    envs.insert(id, media);
  }
}

/// Marks `env_id` as needing a `MediaQueryList` update.
///
/// Returns `true` if the caller should enqueue a task to process the update.
pub(crate) fn queue_match_media_mql_update(env_id: u64) -> bool {
  let mut regs = match_media_mqls().lock();
  let Some(env) = regs.get_mut(&env_id) else {
    return false;
  };
  if env.mqls.is_empty() {
    return false;
  }
  if env.update_task_queued {
    return false;
  }
  env.update_task_queued = true;
  true
}

pub(crate) struct MatchMediaEnvGuard {
  id: u64,
  active: bool,
}

impl MatchMediaEnvGuard {
  pub(crate) fn new(media: MediaContext) -> Self {
    Self {
      id: register_match_media_env(media),
      active: true,
    }
  }

  pub(crate) fn id(&self) -> u64 {
    self.id
  }

  pub(crate) fn disarm(mut self) -> u64 {
    self.active = false;
    self.id
  }
}

impl Drop for MatchMediaEnvGuard {
  fn drop(&mut self) {
    if self.active {
      unregister_match_media_env(self.id);
    }
  }
}

fn prop_key(rt: &mut VmJsRuntime, name: &str) -> Result<PropertyKey, VmError> {
  let Value::String(handle) = rt.alloc_string_value(name)? else {
    return Err(VmError::Unimplemented(
      "alloc_string_value returned non-string",
    ));
  };
  Ok(PropertyKey::String(handle))
}

fn define_read_only_data_property(
  rt: &mut VmJsRuntime,
  obj: Value,
  name: &str,
  value: Value,
  enumerable: bool,
) -> Result<(), VmError> {
  let getter_value = value;
  let getter = rt.alloc_function_value(move |_rt, _this, _args| Ok(getter_value))?;
  let key = prop_key(rt, name)?;
  rt.define_accessor_property(obj, key, getter, Value::Undefined, enumerable)?;
  Ok(())
}

fn define_read_only_number(
  rt: &mut VmJsRuntime,
  obj: Value,
  name: &str,
  value: f64,
) -> Result<(), VmError> {
  define_read_only_data_property(rt, obj, name, Value::Number(value), false)
}

fn define_read_only_bool(
  rt: &mut VmJsRuntime,
  obj: Value,
  name: &str,
  value: bool,
) -> Result<(), VmError> {
  define_read_only_data_property(rt, obj, name, Value::Bool(value), false)
}

fn define_read_only_string(
  rt: &mut VmJsRuntime,
  obj: Value,
  name: &str,
  value: &str,
) -> Result<(), VmError> {
  let js_value = rt.alloc_string_value(value)?;
  define_read_only_data_property(rt, obj, name, js_value, false)
}

fn sanitize_f32_as_f64(value: f32, fallback: f64) -> f64 {
  if value.is_finite() {
    value as f64
  } else {
    fallback
  }
}

fn alloc_key_vm_js(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
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

fn define_read_only_vm_js(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
) -> Result<(), VmError> {
  // Root `obj` and `value` while allocating the property key: `alloc_key_vm_js` can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key_vm_js(&mut scope, name)?;
  scope.define_property(obj, key, read_only_data_desc(value))
}

fn define_enumerable_read_only_vm_js(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
) -> Result<(), VmError> {
  // Root `obj` and `value` while allocating the property key: `alloc_key_vm_js` can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key_vm_js(&mut scope, name)?;
  scope.define_property(
    obj,
    key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value,
        writable: false,
      },
    },
  )
}

fn env_id_from_match_media_callee(scope: &Scope<'_>, callee: GcObject) -> Result<u64, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots
    .get(MATCH_MEDIA_SLOT_ENV_ID)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 => Ok(n as u64),
    _ => Err(VmError::InvariantViolation(
      "matchMedia missing env id native slot",
    )),
  }
}

fn native_call_id_from_match_media_callee(
  scope: &Scope<'_>,
  callee: GcObject,
  slot: usize,
  name: &'static str,
) -> Result<NativeFunctionId, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.get(slot).copied().unwrap_or(Value::Undefined) {
    Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 => Ok(NativeFunctionId(n as u32)),
    _ => Err(VmError::InvariantViolation(name)),
  }
}

fn get_or_create_mql_listeners(
  vm: &Vm,
  scope: &mut Scope<'_>,
  mql: GcObject,
) -> Result<GcObject, VmError> {
  let key = alloc_key_vm_js(scope, MQL_LISTENERS_KEY)?;
  if let Some(Value::Object(obj)) = scope.heap().object_get_own_data_property_value(mql, &key)? {
    return Ok(obj);
  }

  let arr = scope.alloc_array(0)?;
  if let Some(intrinsics) = vm.intrinsics() {
    scope
      .heap_mut()
      .object_set_prototype(arr, Some(intrinsics.array_prototype()))?;
  }
  scope.push_root(Value::Object(arr))?;

  scope.define_property(
    mql,
    key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(arr),
        writable: true,
      },
    },
  )?;

  Ok(arr)
}

fn mql_event_type_is_change(scope: &mut Scope<'_>, value: Value) -> bool {
  // Most real-world usage passes the literal string `"change"`. Avoid allocations for that fast
  // path by comparing UTF-16 code units.
  const CHANGE: [u16; 6] = [99, 104, 97, 110, 103, 101]; // "change"
  match value {
    Value::String(s) => scope
      .heap()
      .get_string(s)
      .ok()
      .is_some_and(|js| js.as_code_units() == CHANGE),
    Value::Null | Value::Undefined => false,
    other => match scope.heap_mut().to_string(other) {
      Ok(s) => scope
        .heap()
        .get_string(s)
        .ok()
        .is_some_and(|js| js.as_code_units() == CHANGE),
      Err(_) => false,
    },
  }
}

fn mql_listener_is_acceptable(_scope: &mut Scope<'_>, listener: Value) -> Result<bool, VmError> {
  // Keep listener registration forgiving: accept any object, and defer `handleEvent` resolution
  // until dispatch time.
  Ok(matches!(listener, Value::Object(_)))
}

fn mql_add_listener_value(
  vm: &Vm,
  scope: &mut Scope<'_>,
  mql: GcObject,
  listener: Value,
) -> Result<(), VmError> {
  if !mql_listener_is_acceptable(scope, listener)? {
    return Ok(());
  }

  let listeners = get_or_create_mql_listeners(vm, scope, mql)?;
  scope.push_root(Value::Object(listeners))?;

  let length_key = alloc_key_vm_js(scope, "length")?;
  let len = match scope.heap().object_get_own_data_property_value(listeners, &length_key)? {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => 0,
  };

  for idx in 0..len {
    let idx_key = alloc_key_vm_js(scope, &idx.to_string())?;
    if let Some(existing) = scope.heap().object_get_own_data_property_value(listeners, &idx_key)? {
      if existing == listener {
        return Ok(());
      }
    }
  }

  let idx_key = alloc_key_vm_js(scope, &len.to_string())?;
  scope.define_property(
    listeners,
    idx_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: listener,
        writable: true,
      },
    },
  )?;
  Ok(())
}

fn mql_remove_listener_value(
  vm: &Vm,
  scope: &mut Scope<'_>,
  mql: GcObject,
  listener: Value,
) -> Result<(), VmError> {
  let Value::Object(_listener_obj) = listener else {
    return Ok(());
  };

  let key = alloc_key_vm_js(scope, MQL_LISTENERS_KEY)?;
  let Some(Value::Object(listeners)) = scope.heap().object_get_own_data_property_value(mql, &key)? else {
    return Ok(());
  };
  scope.push_root(Value::Object(listeners))?;

  let length_key = alloc_key_vm_js(scope, "length")?;
  let len = match scope.heap().object_get_own_data_property_value(listeners, &length_key)? {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => 0,
  };

  // Snapshot and filter.
  let mut remaining: Vec<Value> = Vec::new();
  remaining.try_reserve_exact(len).map_err(|_| VmError::OutOfMemory)?;
  for idx in 0..len {
    let idx_key = alloc_key_vm_js(scope, &idx.to_string())?;
    let v = scope
      .heap()
      .object_get_own_data_property_value(listeners, &idx_key)?
      .unwrap_or(Value::Undefined);
    if v == listener {
      continue;
    }
    remaining.push(v);
  }

  let new_arr = scope.alloc_array(0)?;
  if let Some(intrinsics) = vm.intrinsics() {
    scope
      .heap_mut()
      .object_set_prototype(new_arr, Some(intrinsics.array_prototype()))?;
  }
  scope.push_root(Value::Object(new_arr))?;

  for (idx, v) in remaining.into_iter().enumerate() {
    let idx_key = alloc_key_vm_js(scope, &idx.to_string())?;
    scope.define_property(
      new_arr,
      idx_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data { value: v, writable: true },
      },
    )?;
  }

  scope.define_property(
    mql,
    key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(new_arr),
        writable: true,
      },
    },
  )?;
  Ok(())
}

fn mql_add_event_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(mql) = this else {
    return Ok(Value::Undefined);
  };
  let type_value = args.get(0).copied().unwrap_or(Value::Undefined);
  if !mql_event_type_is_change(scope, type_value) {
    return Ok(Value::Undefined);
  }
  let listener = args.get(1).copied().unwrap_or(Value::Undefined);
  if matches!(listener, Value::Null | Value::Undefined) {
    return Ok(Value::Undefined);
  }
  mql_add_listener_value(vm, scope, mql, listener)?;
  Ok(Value::Undefined)
}

fn mql_remove_event_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(mql) = this else {
    return Ok(Value::Undefined);
  };
  let type_value = args.get(0).copied().unwrap_or(Value::Undefined);
  if !mql_event_type_is_change(scope, type_value) {
    return Ok(Value::Undefined);
  }
  let listener = args.get(1).copied().unwrap_or(Value::Undefined);
  if matches!(listener, Value::Null | Value::Undefined) {
    return Ok(Value::Undefined);
  }
  mql_remove_listener_value(vm, scope, mql, listener)?;
  Ok(Value::Undefined)
}

fn mql_add_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(mql) = this else {
    return Ok(Value::Undefined);
  };
  let listener = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(listener, Value::Null | Value::Undefined) {
    return Ok(Value::Undefined);
  }
  mql_add_listener_value(vm, scope, mql, listener)?;
  Ok(Value::Undefined)
}

fn mql_remove_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(mql) = this else {
    return Ok(Value::Undefined);
  };
  let listener = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(listener, Value::Null | Value::Undefined) {
    return Ok(Value::Undefined);
  }
  mql_remove_listener_value(vm, scope, mql, listener)?;
  Ok(Value::Undefined)
}

fn mql_onchange_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(mql) = this else {
    return Ok(Value::Null);
  };
  let key = alloc_key_vm_js(scope, MQL_ONCHANGE_KEY)?;
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(mql, &key)?
      .unwrap_or(Value::Null),
  )
}

fn mql_onchange_set_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(mql) = this else {
    return Ok(Value::Undefined);
  };
  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  let value = match value {
    Value::Null | Value::Undefined => Value::Null,
    other if scope.heap().is_callable(other).unwrap_or(false) => other,
    _ => Value::Null,
  };

  let key = alloc_key_vm_js(scope, MQL_ONCHANGE_KEY)?;
  scope.define_property(
    mql,
    key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value,
        writable: true,
      },
    },
  )?;
  Ok(Value::Undefined)
}

fn mql_matches_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let env_id = match slots.get(MQL_MATCHES_GET_SLOT_ENV_ID).copied().unwrap_or(Value::Undefined) {
    Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 => n as u64,
    _ => return Ok(Value::Bool(false)),
  };
  let too_long = match slots
    .get(MQL_MATCHES_GET_SLOT_TOO_LONG)
    .copied()
    .unwrap_or(Value::Bool(false))
  {
    Value::Bool(b) => b,
    _ => false,
  };
  if too_long {
    return Ok(Value::Bool(false));
  }
  let query_s = match slots
    .get(MQL_MATCHES_GET_SLOT_QUERY_STRING)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => return Ok(Value::Bool(false)),
  };

  let query_text = match scope.heap().get_string(query_s) {
    Ok(s) => s.to_utf8_lossy(),
    Err(_) => return Ok(Value::Bool(false)),
  };

  let matches = MediaQuery::parse_list(&query_text)
    .ok()
    .and_then(|queries| with_match_media_env(env_id, |ctx| ctx.evaluate_list(&queries)))
    .unwrap_or(false);
  Ok(Value::Bool(matches))
}

fn match_media_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let env_id = env_id_from_match_media_callee(&scope, callee)?;
  let matches_get_call_id = native_call_id_from_match_media_callee(
    &scope,
    callee,
    MATCH_MEDIA_SLOT_MQL_MATCHES_GET_CALL_ID,
    "matchMedia missing matches getter native call id slot",
  )?;
  let add_event_listener_call_id = native_call_id_from_match_media_callee(
    &scope,
    callee,
    MATCH_MEDIA_SLOT_MQL_ADD_EVENT_LISTENER_CALL_ID,
    "matchMedia missing MediaQueryList.addEventListener native call id slot",
  )?;
  let remove_event_listener_call_id = native_call_id_from_match_media_callee(
    &scope,
    callee,
    MATCH_MEDIA_SLOT_MQL_REMOVE_EVENT_LISTENER_CALL_ID,
    "matchMedia missing MediaQueryList.removeEventListener native call id slot",
  )?;
  let add_listener_call_id = native_call_id_from_match_media_callee(
    &scope,
    callee,
    MATCH_MEDIA_SLOT_MQL_ADD_LISTENER_CALL_ID,
    "matchMedia missing MediaQueryList.addListener native call id slot",
  )?;
  let remove_listener_call_id = native_call_id_from_match_media_callee(
    &scope,
    callee,
    MATCH_MEDIA_SLOT_MQL_REMOVE_LISTENER_CALL_ID,
    "matchMedia missing MediaQueryList.removeListener native call id slot",
  )?;
  let onchange_get_call_id = native_call_id_from_match_media_callee(
    &scope,
    callee,
    MATCH_MEDIA_SLOT_MQL_ONCHANGE_GET_CALL_ID,
    "matchMedia missing MediaQueryList.onchange getter native call id slot",
  )?;
  let onchange_set_call_id = native_call_id_from_match_media_callee(
    &scope,
    callee,
    MATCH_MEDIA_SLOT_MQL_ONCHANGE_SET_CALL_ID,
    "matchMedia missing MediaQueryList.onchange setter native call id slot",
  )?;

  let intrinsics = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("matchMedia requires intrinsics"))?;
  let func_proto = intrinsics.function_prototype();

  let query_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let s = match query_value {
    Value::String(s) => s,
    other => scope.heap_mut().to_string(other)?,
  };

  let js_string = scope.heap().get_string(s)?;
  let units = js_string.as_code_units();
  let too_long = units.len() > MAX_MATCH_MEDIA_QUERY_CODE_UNITS;
  let query_text = if too_long {
    String::from_utf16_lossy(&units[..MAX_MATCH_MEDIA_QUERY_CODE_UNITS])
  } else {
    js_string.to_utf8_lossy()
  };

  let media_value = if too_long {
    let truncated = scope.alloc_string(&query_text)?;
    scope.push_root(Value::String(truncated))?;
    Value::String(truncated)
  } else {
    Value::String(s)
  };

  let parsed_queries = if too_long {
    None
  } else {
    MediaQuery::parse_list(&query_text).ok()
  };

  let initial_matches = parsed_queries
    .as_ref()
    .is_some_and(|queries| with_match_media_env(env_id, |ctx| ctx.evaluate_list(queries)).unwrap_or(false));

  let mql = scope.alloc_object()?;
  scope.push_root(Value::Object(mql))?;
  scope.heap_mut().object_set_host_slots(
    mql,
    HostSlots {
      a: MEDIA_QUERY_LIST_HOST_TAG,
      b: 0,
    },
  )?;
  define_read_only_vm_js(&mut scope, mql, "media", media_value)?;

  // `matches` is readonly but dynamic: implement as an accessor that re-evaluates against the
  // current `MediaContext`.
  let matches_get_name = scope.alloc_string("get matches")?;
  scope.push_root(Value::String(matches_get_name))?;
  scope.push_root(media_value)?;
  let matches_get_func = scope.alloc_native_function_with_slots(
    matches_get_call_id,
    None,
    matches_get_name,
    0,
    &[
      Value::Number(env_id as f64),
      Value::Bool(too_long),
      media_value,
    ],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(matches_get_func, Some(func_proto))?;
  scope.push_root(Value::Object(matches_get_func))?;

  let matches_key = alloc_key_vm_js(&mut scope, "matches")?;
  scope.define_property(
    mql,
    matches_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(matches_get_func),
        set: Value::Undefined,
      },
    },
  )?;

  // Listener methods.
  let add_event_listener_name = scope.alloc_string("addEventListener")?;
  scope.push_root(Value::String(add_event_listener_name))?;
  let add_event_listener_func =
    scope.alloc_native_function(add_event_listener_call_id, None, add_event_listener_name, 2)?;
  scope
    .heap_mut()
    .object_set_prototype(add_event_listener_func, Some(func_proto))?;
  scope.push_root(Value::Object(add_event_listener_func))?;
  define_read_only_vm_js(
    &mut scope,
    mql,
    "addEventListener",
    Value::Object(add_event_listener_func),
  )?;

  let remove_event_listener_name = scope.alloc_string("removeEventListener")?;
  scope.push_root(Value::String(remove_event_listener_name))?;
  let remove_event_listener_func = scope.alloc_native_function(
    remove_event_listener_call_id,
    None,
    remove_event_listener_name,
    2,
  )?;
  scope
    .heap_mut()
    .object_set_prototype(remove_event_listener_func, Some(func_proto))?;
  scope.push_root(Value::Object(remove_event_listener_func))?;
  define_read_only_vm_js(
    &mut scope,
    mql,
    "removeEventListener",
    Value::Object(remove_event_listener_func),
  )?;

  let add_listener_name = scope.alloc_string("addListener")?;
  scope.push_root(Value::String(add_listener_name))?;
  let add_listener_func = scope.alloc_native_function(add_listener_call_id, None, add_listener_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(add_listener_func, Some(func_proto))?;
  scope.push_root(Value::Object(add_listener_func))?;
  define_read_only_vm_js(&mut scope, mql, "addListener", Value::Object(add_listener_func))?;

  let remove_listener_name = scope.alloc_string("removeListener")?;
  scope.push_root(Value::String(remove_listener_name))?;
  let remove_listener_func =
    scope.alloc_native_function(remove_listener_call_id, None, remove_listener_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(remove_listener_func, Some(func_proto))?;
  scope.push_root(Value::Object(remove_listener_func))?;
  define_read_only_vm_js(
    &mut scope,
    mql,
    "removeListener",
    Value::Object(remove_listener_func),
  )?;

  // `onchange` EventHandler attribute.
  let onchange_hidden_key = alloc_key_vm_js(&mut scope, MQL_ONCHANGE_KEY)?;
  scope.define_property(
    mql,
    onchange_hidden_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Null,
        writable: true,
      },
    },
  )?;

  let onchange_get_name = scope.alloc_string("get onchange")?;
  scope.push_root(Value::String(onchange_get_name))?;
  let onchange_get_func =
    scope.alloc_native_function(onchange_get_call_id, None, onchange_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(onchange_get_func, Some(func_proto))?;
  scope.push_root(Value::Object(onchange_get_func))?;

  let onchange_set_name = scope.alloc_string("set onchange")?;
  scope.push_root(Value::String(onchange_set_name))?;
  let onchange_set_func =
    scope.alloc_native_function(onchange_set_call_id, None, onchange_set_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(onchange_set_func, Some(func_proto))?;
  scope.push_root(Value::Object(onchange_set_func))?;

  let onchange_key = alloc_key_vm_js(&mut scope, "onchange")?;
  scope.define_property(
    mql,
    onchange_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(onchange_get_func),
        set: Value::Object(onchange_set_func),
      },
    },
  )?;

  register_media_query_list_for_env(
    env_id,
    mql,
    query_text,
    parsed_queries,
    too_long,
    initial_matches,
    scope.heap(),
  );

  Ok(Value::Object(mql))
}

fn register_media_query_list_for_env(
  env_id: u64,
  mql: GcObject,
  query_text: String,
  queries: Option<Vec<MediaQuery>>,
  too_long: bool,
  initial_matches: bool,
  heap: &vm_js::Heap,
) {
  // `matches` is always `false` for too-long inputs or invalid queries; no change events will ever
  // fire, so avoid tracking these and consuming the per-env cap.
  if too_long || queries.is_none() {
    return;
  }

  let mut regs = match_media_mqls().lock();
  let reg = regs.entry(env_id).or_insert_with(|| MatchMediaMqlEnvRegistry {
    mqls: Vec::new(),
    update_task_queued: false,
  });

  // Best-effort cleanup: keep the registry bounded even under scripts that create many temporary
  // MediaQueryLists.
  reg.mqls.retain(|entry| entry.weak.upgrade(heap).is_some());

  if reg.mqls.len() >= MAX_TRACKED_MEDIA_QUERY_LISTS_PER_ENV {
    return;
  }

  reg.mqls.push(TrackedMediaQueryList {
    weak: WeakGcObject::new(mql),
    query_text,
    queries,
    too_long,
    last_matches: initial_matches,
  });
}

/// Process a queued `MediaQueryList` update for `env_id`.
///
/// This recomputes `matches` for all tracked MediaQueryLists, and dispatches `change` events for
/// those whose state toggled.
pub(crate) fn process_match_media_mql_update_for_env(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  env_id: u64,
) -> Result<(), VmError> {
  let Some(media_ctx) = match_media_envs().lock().get(&env_id).cloned() else {
    // Realm already torn down.
    match_media_mqls().lock().remove(&env_id);
    return Ok(());
  };

  #[derive(Clone)]
  struct PendingChange {
    weak: WeakGcObject,
    matches: bool,
    media: String,
  }

  let mut changes: Vec<PendingChange> = Vec::new();

  {
    let mut regs = match_media_mqls().lock();
    let Some(reg) = regs.get_mut(&env_id) else {
      return Ok(());
    };

    // Allow future updates to queue another task even if this dispatch errors out.
    reg.update_task_queued = false;

    // Sweep dead entries and collect changes without holding the lock across JS execution.
    reg.mqls.retain(|entry| entry.weak.upgrade(scope.heap()).is_some());

    for entry in reg.mqls.iter_mut() {
      let new_matches = if entry.too_long {
        false
      } else {
        entry
          .queries
          .as_ref()
          .is_some_and(|queries| media_ctx.evaluate_list(queries))
      };

      if new_matches != entry.last_matches {
        entry.last_matches = new_matches;
        if changes.len() < MAX_MQL_CHANGE_DISPATCHES_PER_ENV_UPDATE {
          changes.push(PendingChange {
            weak: entry.weak,
            matches: new_matches,
            media: entry.query_text.clone(),
          });
        }
      }
    }

    if reg.mqls.is_empty() {
      regs.remove(&env_id);
    }
  }

  for change in changes {
    let Some(mql) = change.weak.upgrade(scope.heap()) else {
      continue;
    };
    dispatch_mql_change_event(vm, scope, host, hooks, mql, change.matches, &change.media)?;
  }

  Ok(())
}

fn dispatch_mql_change_event(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  mql: GcObject,
  matches: bool,
  media: &str,
) -> Result<(), VmError> {
  let mut scope = scope.reborrow();

  // Root `mql` before allocating anything else so a GC triggered by allocation cannot collect the
  // target mid-dispatch.
  scope.push_root(Value::Object(mql))?;

  let event = scope.alloc_object()?;
  scope.push_root(Value::Object(event))?;

  let type_key = alloc_key_vm_js(&mut scope, "type")?;
  let type_s = scope.alloc_string("change")?;
  scope.push_root(Value::String(type_s))?;
  scope.define_property(
    event,
    type_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::String(type_s),
        writable: true,
      },
    },
  )?;

  let matches_key = alloc_key_vm_js(&mut scope, "matches")?;
  scope.define_property(
    event,
    matches_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Bool(matches),
        writable: true,
      },
    },
  )?;

  let media_key = alloc_key_vm_js(&mut scope, "media")?;
  let media_s = scope.alloc_string(media)?;
  scope.push_root(Value::String(media_s))?;
  scope.define_property(
    event,
    media_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::String(media_s),
        writable: true,
      },
    },
  )?;

  // Snapshot listeners.
  let mut listeners: Vec<Value> = Vec::new();
  let listeners_key = alloc_key_vm_js(&mut scope, MQL_LISTENERS_KEY)?;
  if let Some(Value::Object(listeners_obj)) =
    scope.heap().object_get_own_data_property_value(mql, &listeners_key)?
  {
    scope.push_root(Value::Object(listeners_obj))?;
    let length_key = alloc_key_vm_js(&mut scope, "length")?;
    let len = match scope
      .heap()
      .object_get_own_data_property_value(listeners_obj, &length_key)?
    {
      Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
      _ => 0,
    };

    listeners.try_reserve_exact(len).map_err(|_| VmError::OutOfMemory)?;
    for idx in 0..len {
      let idx_key = alloc_key_vm_js(&mut scope, &idx.to_string())?;
      let v = scope
        .heap()
        .object_get_own_data_property_value(listeners_obj, &idx_key)?
        .unwrap_or(Value::Undefined);
      listeners.push(v);
    }
  }

  let event_value = Value::Object(event);

  for listener in listeners {
    let Value::Object(listener_obj) = listener else {
      continue;
    };

    if scope.heap().is_callable(listener).unwrap_or(false) {
      match vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        listener,
        Value::Object(mql),
        &[event_value],
      ) {
        Ok(_) => {}
        Err(e @ VmError::Termination(_)) => return Err(e),
        Err(_) => {}
      }
      continue;
    }

    // EventListener object with `handleEvent`.
    let handle_event_key = alloc_key_vm_js(&mut scope, "handleEvent")?;
    let handle_event = scope.ordinary_get_with_host_and_hooks(
      vm,
      host,
      hooks,
      listener_obj,
      handle_event_key,
      listener,
    )?;
    if matches!(handle_event, Value::Undefined) {
      continue;
    }
    if !scope.heap().is_callable(handle_event).unwrap_or(false) {
      continue;
    }
    match vm.call_with_host_and_hooks(
      host,
      &mut scope,
      hooks,
      handle_event,
      Value::Object(listener_obj),
      &[event_value],
    ) {
      Ok(_) => {}
      Err(e @ VmError::Termination(_)) => return Err(e),
      Err(_) => {}
    }
  }

  // Invoke `onchange` after the listener list for deterministic ordering.
  let onchange_key = alloc_key_vm_js(&mut scope, MQL_ONCHANGE_KEY)?;
  if let Some(handler) = scope
    .heap()
    .object_get_own_data_property_value(mql, &onchange_key)?
  {
    if scope.heap().is_callable(handler).unwrap_or(false) {
      match vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        handler,
        Value::Object(mql),
        &[event_value],
      ) {
        Ok(_) => {}
        Err(e @ VmError::Termination(_)) => return Err(e),
        Err(_) => {}
      }
    }
  }

  Ok(())
}

fn navigator_send_beacon_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let url_value = match args.get(0).copied() {
    Some(v) => v,
    None => return Ok(Value::Bool(false)),
  };

  // `sendBeacon` should accept any URL-ish value by running `ToString`. In `vm-js`, object
  // coercions require the host-aware conversion (`Scope::to_string`) so we can invoke
  // `ToPrimitive`/`toString` when needed.
  //
  // Keep this shim deterministic + non-throwing: swallow normal conversion errors and return
  // `false`, but still propagate VM termination (budget exhaustion, interrupts, etc).
  let url_s = match scope.to_string(vm, host, hooks, url_value) {
    Ok(s) => s,
    Err(e @ VmError::Termination(_)) => return Err(e),
    Err(_) => return Ok(Value::Bool(false)),
  };

  let url_len = match scope.heap().get_string(url_s) {
    Ok(s) => s.as_code_units().len(),
    Err(_) => return Ok(Value::Bool(false)),
  };

  if url_len > MAX_SEND_BEACON_URL_CODE_UNITS {
    return Ok(Value::Bool(false));
  }

  Ok(Value::Bool(true))
}

fn navigator_ua_data_get_high_entropy_values_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("getHighEntropyValues requires intrinsics"))?;

  let slots = scope.heap().get_function_native_slots(callee)?;
  let major_version = slots
    .get(UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_MAJOR_VERSION)
    .copied()
    .unwrap_or(Value::Undefined);
  let full_version = slots
    .get(UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_FULL_VERSION)
    .copied()
    .unwrap_or(Value::Undefined);
  let platform = slots
    .get(UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_PLATFORM)
    .copied()
    .unwrap_or(Value::Undefined);
  let mobile = slots
    .get(UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_MOBILE)
    .copied()
    .unwrap_or(Value::Undefined);
  let platform_version = slots
    .get(UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_PLATFORM_VERSION)
    .copied()
    .unwrap_or(Value::Undefined);
  let architecture = slots
    .get(UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_ARCHITECTURE)
    .copied()
    .unwrap_or(Value::Undefined);
  let bitness = slots
    .get(UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_BITNESS)
    .copied()
    .unwrap_or(Value::Undefined);
  let model = slots
    .get(UA_DATA_GET_HIGH_ENTROPY_VALUES_SLOT_MODEL)
    .copied()
    .unwrap_or(Value::Undefined);
  let major_version_s = match major_version {
    Value::String(s) => s,
    _ => {
      let s = scope.alloc_string("0")?;
      scope.push_root(Value::String(s))?;
      s
    }
  };
  let full_version_s = match full_version {
    Value::String(s) => s,
    _ => {
      let s = scope.alloc_string("0.0.0.0")?;
      scope.push_root(Value::String(s))?;
      s
    }
  };
  let platform_s = match platform {
    Value::String(s) => s,
    _ => {
      let s = scope.alloc_string("Windows")?;
      scope.push_root(Value::String(s))?;
      s
    }
  };
  let mobile_b = matches!(mobile, Value::Bool(true));
  let platform_version_s = match platform_version {
    Value::String(s) => s,
    _ => {
      let s = scope.alloc_string("0.0.0")?;
      scope.push_root(Value::String(s))?;
      s
    }
  };
  let architecture_s = match architecture {
    Value::String(s) => s,
    _ => {
      let s = scope.alloc_string("x86")?;
      scope.push_root(Value::String(s))?;
      s
    }
  };
  let bitness_s = match bitness {
    Value::String(s) => s,
    _ => {
      let s = scope.alloc_string("64")?;
      scope.push_root(Value::String(s))?;
      s
    }
  };
  let model_s = match model {
    Value::String(s) => s,
    _ => {
      let s = scope.alloc_string("")?;
      scope.push_root(Value::String(s))?;
      s
    }
  };

  // Parse the requested hint list. This is intentionally forgiving/non-throwing: bad hint inputs
  // should just be ignored (real-world usage often probes without validating supported hints).
  let mut want_brands = false;
  let mut want_mobile = false;
  let mut want_platform = false;
  let mut want_platform_version = false;
  let mut want_architecture = false;
  let mut want_bitness = false;
  let mut want_model = false;
  let mut want_ua_full_version = false;
  let mut want_form_factors = false;

  if let Some(Value::Object(hints_obj)) = args.get(0).copied() {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(hints_obj))?;

    let length_key = alloc_key_vm_js(&mut scope, "length")?;
    let length_value = match scope.ordinary_get_with_host_and_hooks(
      vm,
      host,
      hooks,
      hints_obj,
      length_key,
      Value::Object(hints_obj),
    ) {
      Ok(v) => v,
      Err(e @ VmError::Termination(_)) => return Err(e),
      Err(_) => Value::Undefined,
    };

    let len = match scope.to_length(vm, host, hooks, length_value) {
      Ok(n) => n.min(MAX_UA_DATA_HINTS),
      Err(e @ VmError::Termination(_)) => return Err(e),
      Err(_) => 0,
    };

    for idx in 0..len {
      let idx_key = alloc_key_vm_js(&mut scope, &idx.to_string())?;
      let hint_value = match scope.ordinary_get_with_host_and_hooks(
        vm,
        host,
        hooks,
        hints_obj,
        idx_key,
        Value::Object(hints_obj),
      ) {
        Ok(v) => v,
        Err(e @ VmError::Termination(_)) => return Err(e),
        Err(_) => continue,
      };

      let hint_s = match scope.to_string(vm, host, hooks, hint_value) {
        Ok(s) => s,
        Err(e @ VmError::Termination(_)) => return Err(e),
        Err(_) => continue,
      };
      let hint_js = match scope.heap().get_string(hint_s) {
        Ok(s) => s,
        Err(_) => continue,
      };
      if hint_js.as_code_units().len() > MAX_UA_DATA_HINT_STRING_CODE_UNITS {
        continue;
      }
      let hint = hint_js.to_utf8_lossy();

      match hint.as_str() {
        "brands" => want_brands = true,
        "mobile" => want_mobile = true,
        "platform" => want_platform = true,
        "platformVersion" => want_platform_version = true,
        "architecture" => want_architecture = true,
        "bitness" => want_bitness = true,
        "model" => want_model = true,
        "uaFullVersion" => want_ua_full_version = true,
        "formFactors" => want_form_factors = true,
        // `fullVersionList` is always returned for forgiveness, even if not requested.
        _ => {}
      }
    }
  }

  // Always return `fullVersionList`; this keeps the shim forgiving for real-world usage which may
  // probe without validating supported hints.
  let full_version_list = scope.alloc_array(3)?;
  scope.push_root(Value::Object(full_version_list))?;
  scope
    .heap_mut()
    .object_set_prototype(full_version_list, Some(intr.array_prototype()))?;

  for (idx, (brand, version)) in [
    ("Not.A/Brand", "99.0.0.0"),
    ("Chromium", ""),
    ("Google Chrome", ""),
  ]
  .into_iter()
  .enumerate()
  {
    let entry = scope.alloc_object()?;
    scope.push_root(Value::Object(entry))?;
    scope
      .heap_mut()
      .object_set_prototype(entry, Some(intr.object_prototype()))?;

    let brand_s = scope.alloc_string(brand)?;
    scope.push_root(Value::String(brand_s))?;
    define_enumerable_read_only_vm_js(scope, entry, "brand", Value::String(brand_s))?;

    let version_s = if version.is_empty() {
      full_version_s
    } else {
      let s = scope.alloc_string(version)?;
      scope.push_root(Value::String(s))?;
      s
    };
    define_enumerable_read_only_vm_js(scope, entry, "version", Value::String(version_s))?;

    let idx_key = alloc_key_vm_js(scope, &idx.to_string())?;
    scope.define_property(
      full_version_list,
      idx_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Object(entry),
          writable: false,
        },
      },
    )?;
  }

  let result = scope.alloc_object()?;
  scope.push_root(Value::Object(result))?;
  scope
    .heap_mut()
    .object_set_prototype(result, Some(intr.object_prototype()))?;

  define_enumerable_read_only_vm_js(
    scope,
    result,
    "fullVersionList",
    Value::Object(full_version_list),
  )?;

  if want_brands {
    let brands = scope.alloc_array(3)?;
    scope.push_root(Value::Object(brands))?;
    scope
      .heap_mut()
      .object_set_prototype(brands, Some(intr.array_prototype()))?;

    for (idx, (brand, version)) in [
      ("Not.A/Brand", "99"),
      ("Chromium", ""),
      ("Google Chrome", ""),
    ]
    .into_iter()
    .enumerate()
    {
      let entry = scope.alloc_object()?;
      scope.push_root(Value::Object(entry))?;
      scope
        .heap_mut()
        .object_set_prototype(entry, Some(intr.object_prototype()))?;

      let brand_s = scope.alloc_string(brand)?;
      scope.push_root(Value::String(brand_s))?;
      define_enumerable_read_only_vm_js(scope, entry, "brand", Value::String(brand_s))?;

      let version_s = if version.is_empty() {
        major_version_s
      } else {
        let s = scope.alloc_string(version)?;
        scope.push_root(Value::String(s))?;
        s
      };
      define_enumerable_read_only_vm_js(scope, entry, "version", Value::String(version_s))?;

      let idx_key = alloc_key_vm_js(scope, &idx.to_string())?;
      scope.define_property(
        brands,
        idx_key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(entry),
            writable: false,
          },
        },
      )?;
    }

    define_enumerable_read_only_vm_js(scope, result, "brands", Value::Object(brands))?;
  }

  if want_mobile {
    define_enumerable_read_only_vm_js(scope, result, "mobile", Value::Bool(mobile_b))?;
  }
  if want_platform {
    define_enumerable_read_only_vm_js(scope, result, "platform", Value::String(platform_s))?;
  }
  if want_platform_version {
    define_enumerable_read_only_vm_js(
      scope,
      result,
      "platformVersion",
      Value::String(platform_version_s),
    )?;
  }
  if want_architecture {
    define_enumerable_read_only_vm_js(
      scope,
      result,
      "architecture",
      Value::String(architecture_s),
    )?;
  }
  if want_bitness {
    define_enumerable_read_only_vm_js(scope, result, "bitness", Value::String(bitness_s))?;
  }
  if want_model {
    define_enumerable_read_only_vm_js(scope, result, "model", Value::String(model_s))?;
  }
  if want_ua_full_version {
    define_enumerable_read_only_vm_js(
      scope,
      result,
      "uaFullVersion",
      Value::String(full_version_s),
    )?;
  }
  if want_form_factors {
    let form_factors = scope.alloc_array(1)?;
    scope.push_root(Value::Object(form_factors))?;
    scope
      .heap_mut()
      .object_set_prototype(form_factors, Some(intr.array_prototype()))?;
    let ff_value = if mobile_b { "Mobile" } else { "Desktop" };
    let ff_s = scope.alloc_string(ff_value)?;
    scope.push_root(Value::String(ff_s))?;
    let idx_key = alloc_key_vm_js(scope, "0")?;
    scope.define_property(
      form_factors,
      idx_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::String(ff_s),
          writable: false,
        },
      },
    )?;
    define_enumerable_read_only_vm_js(scope, result, "formFactors", Value::Object(form_factors))?;
  }

  // Ensure the slot values remain referenced even if the caller only requests a subset of fields.
  let _ = (major_version_s, platform_s, platform_version_s, architecture_s, bitness_s, model_s);

  // Resolve synchronously.
  let promise = promise_resolve_with_host_and_hooks(vm, scope, host, hooks, Value::Object(result))?;
  Ok(promise)
}

/// Installs basic browser-environment shims onto a `vm-js` Window realm global object.
///
/// Returns a host-side environment ID that must be unregistered with
/// [`unregister_match_media_env`] when the realm is torn down.
pub(crate) fn install_window_shims_vm_js(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  realm: &Realm,
  window: GcObject,
  env: WindowEnv,
  match_media_env_id: u64,
) -> Result<(), VmError> {
  let viewport_width = sanitize_f32_as_f64(env.media.viewport_width, 0.0);
  let viewport_height = sanitize_f32_as_f64(env.media.viewport_height, 0.0);
  let device_width = sanitize_f32_as_f64(env.media.device_width, viewport_width);
  let device_height = sanitize_f32_as_f64(env.media.device_height, viewport_height);
  let dpr = sanitize_f32_as_f64(env.media.device_pixel_ratio, 1.0);
  let ua_data_info = ua_data_info_from_env(&env);

  define_read_only_vm_js(scope, window, "devicePixelRatio", Value::Number(dpr))?;
  define_read_only_vm_js(scope, window, "innerWidth", Value::Number(viewport_width))?;
  define_read_only_vm_js(scope, window, "innerHeight", Value::Number(viewport_height))?;
  define_read_only_vm_js(scope, window, "outerWidth", Value::Number(viewport_width))?;
  define_read_only_vm_js(scope, window, "outerHeight", Value::Number(viewport_height))?;

  let screen = scope.alloc_object()?;
  scope.push_root(Value::Object(screen))?;
  scope.heap_mut().object_set_host_slots(
    screen,
    HostSlots {
      a: SCREEN_HOST_TAG,
      b: 0,
    },
  )?;
  define_read_only_vm_js(scope, screen, "width", Value::Number(device_width))?;
  define_read_only_vm_js(scope, screen, "height", Value::Number(device_height))?;
  define_read_only_vm_js(scope, screen, "availWidth", Value::Number(device_width))?;
  define_read_only_vm_js(scope, screen, "availHeight", Value::Number(device_height))?;
  define_read_only_vm_js(scope, window, "screen", Value::Object(screen))?;

  let navigator = scope.alloc_object()?;
  scope.push_root(Value::Object(navigator))?;
  scope.heap_mut().object_set_host_slots(
    navigator,
    HostSlots {
      a: NAVIGATOR_HOST_TAG,
      b: 0,
    },
  )?;
  let user_agent_s = scope.alloc_string(env.user_agent)?;
  scope.push_root(Value::String(user_agent_s))?;
  define_read_only_vm_js(scope, navigator, "userAgent", Value::String(user_agent_s))?;
  let platform_s = scope.alloc_string(env.platform)?;
  scope.push_root(Value::String(platform_s))?;
  define_read_only_vm_js(scope, navigator, "platform", Value::String(platform_s))?;
  let language_s = scope.alloc_string(env.language)?;
  scope.push_root(Value::String(language_s))?;
  define_read_only_vm_js(scope, navigator, "language", Value::String(language_s))?;

  // High-signal `Navigator` feature-detection fields that many real-world sites probe.
  //
  // Keep these deterministic (do not sniff the host machine), read-only, and non-throwing.
  define_read_only_vm_js(scope, navigator, "onLine", Value::Bool(true))?;
  define_read_only_vm_js(scope, navigator, "cookieEnabled", Value::Bool(true))?;
  define_read_only_vm_js(scope, navigator, "hardwareConcurrency", Value::Number(4.0))?;
  define_read_only_vm_js(scope, navigator, "deviceMemory", Value::Number(8.0))?;

  // `navigator.languages` is a `FrozenArray<DOMString>` in browsers. Model it as a real JS Array so
  // feature detection (`Array.isArray`) and common methods (`includes`, `join`) work as expected.
  let languages = scope.alloc_array(env.languages.len())?;
  scope.push_root(Value::Object(languages))?;
  scope
    .heap_mut()
    .object_set_prototype(languages, Some(realm.intrinsics().array_prototype()))?;
  for (idx, lang) in env.languages.iter().enumerate() {
    let idx_key = alloc_key_vm_js(scope, &idx.to_string())?;
    let lang_s = scope.alloc_string(lang)?;
    scope.push_root(Value::String(lang_s))?;
    scope.define_property(
      languages,
      idx_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::String(lang_s),
          writable: false,
        },
      },
    )?;
  }
  define_read_only_vm_js(scope, navigator, "languages", Value::Object(languages))?;

  // UA Client Hints: `navigator.userAgentData` (NavigatorUAData).
  //
  // Many real-world sites probe this surface unguarded; keep it deterministic and forgiving.
  let user_agent_data = scope.alloc_object()?;
  scope.push_root(Value::Object(user_agent_data))?;
  scope.heap_mut().object_set_host_slots(
    user_agent_data,
    HostSlots {
      a: USER_AGENT_DATA_HOST_TAG,
      b: 0,
    },
  )?;
  scope
    .heap_mut()
    .object_set_prototype(user_agent_data, Some(realm.intrinsics().object_prototype()))?;

  let ua_platform_s = scope.alloc_string(&ua_data_info.platform)?;
  scope.push_root(Value::String(ua_platform_s))?;
  define_read_only_vm_js(
    scope,
    user_agent_data,
    "platform",
    Value::String(ua_platform_s),
  )?;
  define_read_only_vm_js(scope, user_agent_data, "mobile", Value::Bool(ua_data_info.mobile))?;

  let major_version_s = scope.alloc_string(&ua_data_info.major_version)?;
  scope.push_root(Value::String(major_version_s))?;
  let full_version_s = scope.alloc_string(&ua_data_info.full_version)?;
  scope.push_root(Value::String(full_version_s))?;
  let platform_version_s = scope.alloc_string(ua_data_platform_version(&ua_data_info.platform))?;
  scope.push_root(Value::String(platform_version_s))?;
  let architecture_s = scope.alloc_string("x86")?;
  scope.push_root(Value::String(architecture_s))?;
  let bitness_s = scope.alloc_string("64")?;
  scope.push_root(Value::String(bitness_s))?;
  let model_s = scope.alloc_string("")?;
  scope.push_root(Value::String(model_s))?;

  let brands = scope.alloc_array(3)?;
  scope.push_root(Value::Object(brands))?;
  scope
    .heap_mut()
    .object_set_prototype(brands, Some(realm.intrinsics().array_prototype()))?;

  for (idx, (brand, version)) in [
    ("Not.A/Brand", "99"),
    ("Chromium", ""),
    ("Google Chrome", ""),
  ]
  .into_iter()
  .enumerate()
  {
    let entry = scope.alloc_object()?;
    scope.push_root(Value::Object(entry))?;
    scope
      .heap_mut()
      .object_set_prototype(entry, Some(realm.intrinsics().object_prototype()))?;

    let brand_s = scope.alloc_string(brand)?;
    scope.push_root(Value::String(brand_s))?;
    define_enumerable_read_only_vm_js(scope, entry, "brand", Value::String(brand_s))?;

    let version_s = if version.is_empty() {
      major_version_s
    } else {
      let s = scope.alloc_string(version)?;
      scope.push_root(Value::String(s))?;
      s
    };
    define_enumerable_read_only_vm_js(scope, entry, "version", Value::String(version_s))?;

    let idx_key = alloc_key_vm_js(scope, &idx.to_string())?;
    scope.define_property(
      brands,
      idx_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Object(entry),
          writable: false,
        },
      },
    )?;
  }

  define_read_only_vm_js(scope, user_agent_data, "brands", Value::Object(brands))?;

  let ghev_call_id = vm.register_native_call(navigator_ua_data_get_high_entropy_values_native)?;
  let ghev_name = scope.alloc_string("getHighEntropyValues")?;
  scope.push_root(Value::String(ghev_name))?;
  let ghev_func = scope.alloc_native_function_with_slots(
    ghev_call_id,
    None,
    ghev_name,
    1,
    &[
      Value::String(major_version_s),
      Value::String(full_version_s),
      Value::String(ua_platform_s),
      Value::Bool(ua_data_info.mobile),
      Value::String(platform_version_s),
      Value::String(architecture_s),
      Value::String(bitness_s),
      Value::String(model_s),
    ],
  )?;
  scope.heap_mut().object_set_prototype(
    ghev_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(ghev_func))?;
  define_read_only_vm_js(
    scope,
    user_agent_data,
    "getHighEntropyValues",
    Value::Object(ghev_func),
  )?;

  define_read_only_vm_js(
    scope,
    navigator,
    "userAgentData",
    Value::Object(user_agent_data),
  )?;

  let send_beacon_call_id = vm.register_native_call(navigator_send_beacon_native)?;
  let send_beacon_name = scope.alloc_string("sendBeacon")?;
  scope.push_root(Value::String(send_beacon_name))?;
  let send_beacon_func =
    scope.alloc_native_function(send_beacon_call_id, None, send_beacon_name, 2)?;
  scope.heap_mut().object_set_prototype(
    send_beacon_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(send_beacon_func))?;
  define_read_only_vm_js(
    scope,
    navigator,
    "sendBeacon",
    Value::Object(send_beacon_func),
  )?;

  define_read_only_vm_js(scope, window, "navigator", Value::Object(navigator))?;

  // `matchMedia` / `MediaQueryList` shims.
  let mql_matches_get_call_id = vm.register_native_call(mql_matches_get_native)?;
  let mql_add_event_listener_call_id = vm.register_native_call(mql_add_event_listener_native)?;
  let mql_remove_event_listener_call_id = vm.register_native_call(mql_remove_event_listener_native)?;
  let mql_add_listener_call_id = vm.register_native_call(mql_add_listener_native)?;
  let mql_remove_listener_call_id = vm.register_native_call(mql_remove_listener_native)?;
  let mql_onchange_get_call_id = vm.register_native_call(mql_onchange_get_native)?;
  let mql_onchange_set_call_id = vm.register_native_call(mql_onchange_set_native)?;

  let match_media_call_id = vm.register_native_call(match_media_native)?;
  let match_media_name = scope.alloc_string("matchMedia")?;
  scope.push_root(Value::String(match_media_name))?;
  let match_media_func = scope.alloc_native_function_with_slots(
    match_media_call_id,
    None,
    match_media_name,
    1,
    &[
      Value::Number(match_media_env_id as f64),
      Value::Number(mql_matches_get_call_id.0 as f64),
      Value::Number(mql_add_event_listener_call_id.0 as f64),
      Value::Number(mql_remove_event_listener_call_id.0 as f64),
      Value::Number(mql_add_listener_call_id.0 as f64),
      Value::Number(mql_remove_listener_call_id.0 as f64),
      Value::Number(mql_onchange_get_call_id.0 as f64),
      Value::Number(mql_onchange_set_call_id.0 as f64),
    ],
  )?;
  scope.heap_mut().object_set_prototype(
    match_media_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(match_media_func))?;

  define_read_only_vm_js(scope, window, "matchMedia", Value::Object(match_media_func))?;

  Ok(())
}

/// Installs basic browser-environment shims onto a window-like global object.
///
/// The installed surface is intentionally minimal and deterministic:
/// - `window.devicePixelRatio`
/// - viewport geometry (`innerWidth`/`innerHeight`, `outerWidth`/`outerHeight`, `screen.*`)
/// - `navigator` (`userAgent`, `platform`, `language`, `languages`, `onLine`, `cookieEnabled`,
///   `hardwareConcurrency`, `deviceMemory`)
/// - `matchMedia(query)` returning a `MediaQueryList`-like object (`matches`, `media`)
pub fn install_window_shims(
  rt: &mut VmJsRuntime,
  window: Value,
  env: WindowEnv,
) -> Result<(), VmError> {
  let viewport_width = sanitize_f32_as_f64(env.media.viewport_width, 0.0);
  let viewport_height = sanitize_f32_as_f64(env.media.viewport_height, 0.0);
  let device_width = sanitize_f32_as_f64(env.media.device_width, viewport_width);
  let device_height = sanitize_f32_as_f64(env.media.device_height, viewport_height);
  let dpr = sanitize_f32_as_f64(env.media.device_pixel_ratio, 1.0);
  let ua_data_info = ua_data_info_from_env(&env);

  define_read_only_number(rt, window, "devicePixelRatio", dpr)?;

  define_read_only_number(rt, window, "innerWidth", viewport_width)?;
  define_read_only_number(rt, window, "innerHeight", viewport_height)?;
  define_read_only_number(rt, window, "outerWidth", viewport_width)?;
  define_read_only_number(rt, window, "outerHeight", viewport_height)?;

  // `screen` object (minimal).
  let screen = rt.alloc_object_value()?;
  define_read_only_number(rt, screen, "width", device_width)?;
  define_read_only_number(rt, screen, "height", device_height)?;
  define_read_only_number(rt, screen, "availWidth", device_width)?;
  define_read_only_number(rt, screen, "availHeight", device_height)?;
  let screen_key = prop_key(rt, "screen")?;
  rt.define_data_property(window, screen_key, screen, false)?;

  // `navigator` object (minimal).
  let navigator = rt.alloc_object_value()?;
  define_read_only_string(rt, navigator, "userAgent", env.user_agent)?;
  define_read_only_string(rt, navigator, "platform", env.platform)?;
  define_read_only_string(rt, navigator, "language", env.language)?;
  define_read_only_bool(rt, navigator, "onLine", true)?;
  define_read_only_bool(rt, navigator, "cookieEnabled", true)?;
  define_read_only_number(rt, navigator, "hardwareConcurrency", 4.0)?;
  define_read_only_number(rt, navigator, "deviceMemory", 8.0)?;

  // UA Client Hints: `navigator.userAgentData` (NavigatorUAData).
  //
  // The legacy `VmJsRuntime` shim does not provide a full JS execution environment, but we still
  // expose a spec-ish shape so host-driven callers and fixture scripts that probe this field can
  // run against both runtimes.
  let user_agent_data = rt.alloc_object_value()?;
  define_read_only_string(rt, user_agent_data, "platform", &ua_data_info.platform)?;
  define_read_only_bool(rt, user_agent_data, "mobile", ua_data_info.mobile)?;

  // `brands`: an Array of `{ brand, version }` objects.
  let brands = rt.alloc_array()?;
  for (idx, (brand, version)) in [
    ("Not.A/Brand", "99"),
    ("Chromium", ua_data_info.major_version.as_str()),
    ("Google Chrome", ua_data_info.major_version.as_str()),
  ]
  .into_iter()
  .enumerate()
  {
    let entry = rt.alloc_object_value()?;
    let brand_value = rt.alloc_string_value(brand)?;
    define_read_only_data_property(
      rt,
      entry,
      "brand",
      brand_value,
      true,
    )?;
    let version_value = rt.alloc_string_value(version)?;
    define_read_only_data_property(
      rt,
      entry,
      "version",
      version_value,
      true,
    )?;
    let idx_key = prop_key(rt, &idx.to_string())?;
    rt.define_data_property(brands, idx_key, entry, true)?;
  }
  let brands_key = prop_key(rt, "brands")?;
  rt.define_data_property(user_agent_data, brands_key, brands, false)?;

  // `getHighEntropyValues(hints)`: return a thenable that resolves immediately.
  // (We cannot create a real `%Promise%` without a Realm.)
  let ua_platform = ua_data_info.platform.clone();
  let ua_platform_version = ua_data_platform_version(&ua_platform).to_string();
  let ua_mobile = ua_data_info.mobile;
  let chrome_major = ua_data_info.major_version.clone();
  let chrome_full = ua_data_info.full_version.clone();
  let get_high_entropy_values = rt.alloc_function_value(move |rt, _this, args| {
    // See vm-js variant above: be forgiving, ignore invalid hints, and bound work.
    let mut want_brands = false;
    let mut want_mobile = false;
    let mut want_platform = false;
    let mut want_platform_version = false;
    let mut want_architecture = false;
    let mut want_bitness = false;
    let mut want_model = false;
    let mut want_ua_full_version = false;
    let mut want_form_factors = false;

    if let Some(Value::Object(hints_obj)) = args.get(0).copied() {
      let length_key = prop_key(rt, "length")?;
      let len = match rt.get(Value::Object(hints_obj), length_key) {
        Ok(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
        _ => 0,
      }
      .min(MAX_UA_DATA_HINTS);

      for idx in 0..len {
        let idx_key = prop_key(rt, &idx.to_string())?;
        let hint_value = rt.get(Value::Object(hints_obj), idx_key).unwrap_or(Value::Undefined);
        let hint_s = match rt.to_string(hint_value) {
          Ok(Value::String(s)) => s,
          _ => continue,
        };
        let hint_js = match rt.heap().get_string(hint_s) {
          Ok(s) => s,
          Err(_) => continue,
        };
        if hint_js.as_code_units().len() > MAX_UA_DATA_HINT_STRING_CODE_UNITS {
          continue;
        }
        let hint = hint_js.to_utf8_lossy();

        match hint.as_str() {
          "brands" => want_brands = true,
          "mobile" => want_mobile = true,
          "platform" => want_platform = true,
          "platformVersion" => want_platform_version = true,
          "architecture" => want_architecture = true,
          "bitness" => want_bitness = true,
          "model" => want_model = true,
          "uaFullVersion" => want_ua_full_version = true,
          "formFactors" => want_form_factors = true,
          // `fullVersionList` is always returned for forgiveness.
          _ => {}
        }
      }
    }

    let thenable = rt.alloc_object_value()?;
    let ua_platform = ua_platform.clone();
    let ua_platform_version = ua_platform_version.clone();
    let chrome_major = chrome_major.clone();
    let chrome_full = chrome_full.clone();

    let then_fn = rt.alloc_function_value(move |rt, _this, args| {
      let on_fulfilled = args.get(0).copied().unwrap_or(Value::Undefined);

      let result = rt.alloc_object_value()?;

      // Always include `fullVersionList` for real-world compatibility.
      let full_version_list = rt.alloc_array()?;
      for (idx, (brand, version)) in [
        ("Not.A/Brand", "99.0.0.0".to_string()),
        ("Chromium", chrome_full.clone()),
        ("Google Chrome", chrome_full.clone()),
      ]
      .into_iter()
      .enumerate()
      {
        let entry = rt.alloc_object_value()?;
        let brand_value = rt.alloc_string_value(brand)?;
        define_read_only_data_property(rt, entry, "brand", brand_value, true)?;
        let version_value = rt.alloc_string_value(&version)?;
        define_read_only_data_property(rt, entry, "version", version_value, true)?;
        let idx_key = prop_key(rt, &idx.to_string())?;
        rt.define_data_property(full_version_list, idx_key, entry, true)?;
      }
      let full_version_list_key = prop_key(rt, "fullVersionList")?;
      rt.define_data_property(result, full_version_list_key, full_version_list, true)?;

      if want_brands {
        let brands = rt.alloc_array()?;
        for (idx, (brand, version)) in [
          ("Not.A/Brand", "99".to_string()),
          ("Chromium", chrome_major.clone()),
          ("Google Chrome", chrome_major.clone()),
        ]
        .into_iter()
        .enumerate()
        {
          let entry = rt.alloc_object_value()?;
          let brand_value = rt.alloc_string_value(brand)?;
          define_read_only_data_property(rt, entry, "brand", brand_value, true)?;
          let version_value = rt.alloc_string_value(&version)?;
          define_read_only_data_property(rt, entry, "version", version_value, true)?;
          let idx_key = prop_key(rt, &idx.to_string())?;
          rt.define_data_property(brands, idx_key, entry, true)?;
        }
        let key = prop_key(rt, "brands")?;
        rt.define_data_property(result, key, brands, true)?;
      }

      if want_mobile {
        let key = prop_key(rt, "mobile")?;
        rt.define_data_property(result, key, Value::Bool(ua_mobile), true)?;
      }
      if want_platform {
        let key = prop_key(rt, "platform")?;
        let v = rt.alloc_string_value(&ua_platform)?;
        rt.define_data_property(result, key, v, true)?;
      }
      if want_platform_version {
        let key = prop_key(rt, "platformVersion")?;
        let v = rt.alloc_string_value(&ua_platform_version)?;
        rt.define_data_property(result, key, v, true)?;
      }
      if want_architecture {
        let key = prop_key(rt, "architecture")?;
        let v = rt.alloc_string_value("x86")?;
        rt.define_data_property(result, key, v, true)?;
      }
      if want_bitness {
        let key = prop_key(rt, "bitness")?;
        let v = rt.alloc_string_value("64")?;
        rt.define_data_property(result, key, v, true)?;
      }
      if want_model {
        let key = prop_key(rt, "model")?;
        let v = rt.alloc_string_value("")?;
        rt.define_data_property(result, key, v, true)?;
      }
      if want_ua_full_version {
        let key = prop_key(rt, "uaFullVersion")?;
        let v = rt.alloc_string_value(&chrome_full)?;
        rt.define_data_property(result, key, v, true)?;
      }
      if want_form_factors {
        let factors = rt.alloc_array()?;
        let ff = if ua_mobile { "Mobile" } else { "Desktop" };
        let v = rt.alloc_string_value(ff)?;
        let idx_key = prop_key(rt, "0")?;
        rt.define_data_property(factors, idx_key, v, true)?;
        let key = prop_key(rt, "formFactors")?;
        rt.define_data_property(result, key, factors, true)?;
      }

      // If `on_fulfilled` is callable, invoke it with the resolved value.
      if rt.is_callable(on_fulfilled) {
        let _ = rt.call_function(on_fulfilled, Value::Undefined, &[result]);
      }

      Ok(Value::Undefined)
    })?;

    let then_key = prop_key(rt, "then")?;
    rt.define_data_property(thenable, then_key, then_fn, false)?;
    Ok(thenable)
  })?;
  let ghev_key = prop_key(rt, "getHighEntropyValues")?;
  rt.define_data_property(user_agent_data, ghev_key, get_high_entropy_values, false)?;

  let user_agent_data_key = prop_key(rt, "userAgentData")?;
  rt.define_data_property(navigator, user_agent_data_key, user_agent_data, false)?;

  let send_beacon = rt.alloc_function_value(|rt, _this, args| {
    let url_value = match args.get(0).copied() {
      Some(v) => v,
      None => return Ok(Value::Bool(false)),
    };

    let s_value = match rt.to_string(url_value) {
      Ok(v) => v,
      Err(_) => return Ok(Value::Bool(false)),
    };
    let Value::String(handle) = s_value else {
      return Ok(Value::Bool(false));
    };

    let url_len = match rt.heap().get_string(handle) {
      Ok(s) => s.as_code_units().len(),
      Err(_) => return Ok(Value::Bool(false)),
    };

    if url_len > MAX_SEND_BEACON_URL_CODE_UNITS {
      return Ok(Value::Bool(false));
    }

    Ok(Value::Bool(true))
  })?;
  let send_beacon_key = prop_key(rt, "sendBeacon")?;
  rt.define_data_property(navigator, send_beacon_key, send_beacon, false)?;

  // `navigator.languages` is an array in browsers; represent it as a tiny array-like object.
  let languages = rt.alloc_object_value()?;
  for (idx, lang) in env.languages.iter().enumerate() {
    let idx_key = prop_key(rt, &idx.to_string())?;
    let lang_value = rt.alloc_string_value(lang)?;
    rt.define_data_property(languages, idx_key, lang_value, true)?;
  }
  define_read_only_number(rt, languages, "length", env.languages.len() as f64)?;
  let languages_key = prop_key(rt, "languages")?;
  rt.define_data_property(navigator, languages_key, languages, false)?;

  let navigator_key = prop_key(rt, "navigator")?;
  rt.define_data_property(window, navigator_key, navigator, false)?;

  // `matchMedia(query)` implementation.
  let media_ctx = env.media.clone();
  let match_media = rt.alloc_function_value(move |rt, _this, args| {
    let arg = args.get(0).copied().unwrap_or(Value::Undefined);
    let s_value = rt.to_string(arg)?;
    let Value::String(handle) = s_value else {
      return Err(VmError::Unimplemented("ToString returned non-string"));
    };

    let js_string = rt.heap().get_string(handle)?;
    let units = js_string.as_code_units();
    let too_long = units.len() > MAX_MATCH_MEDIA_QUERY_CODE_UNITS;
    let query_text = if too_long {
      // Bound work/memory use. We still return a `MediaQueryList` object, but treat the
      // query as invalid.
      String::from_utf16_lossy(&units[..MAX_MATCH_MEDIA_QUERY_CODE_UNITS])
    } else {
      js_string.to_utf8_lossy()
    };

    let (matches, media_value) = if too_long {
      (false, rt.alloc_string_value(&query_text)?)
    } else {
      let matches = MediaQuery::parse_list(&query_text)
        .ok()
        .is_some_and(|queries| media_ctx.evaluate_list(&queries));
      // Echo the original query string by reusing the JS string handle (bounded by length check).
      (matches, Value::String(handle))
    };

    let mql = rt.alloc_object_value()?;
    define_read_only_bool(rt, mql, "matches", matches)?;
    let media_key = prop_key(rt, "media")?;
    rt.define_data_property(mql, media_key, media_value, false)?;

    let noop = rt.alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))?;
    let add_listener_key = prop_key(rt, "addListener")?;
    let remove_listener_key = prop_key(rt, "removeListener")?;
    let add_event_key = prop_key(rt, "addEventListener")?;
    let remove_event_key = prop_key(rt, "removeEventListener")?;
    rt.define_data_property(mql, add_listener_key, noop, false)?;
    rt.define_data_property(mql, remove_listener_key, noop, false)?;
    rt.define_data_property(mql, add_event_key, noop, false)?;
    rt.define_data_property(mql, remove_event_key, noop, false)?;

    Ok(mql)
  })?;

  let match_media_key = prop_key(rt, "matchMedia")?;
  rt.define_data_property(window, match_media_key, match_media, false)?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom2;
  use crate::js::{RunLimits, RunUntilIdleOutcome, WindowHost};
  use crate::style::media::MediaContext;
  use selectors::context::QuirksMode;
  use std::time::Duration;

  fn get_prop(rt: &mut VmJsRuntime, obj: Value, name: &str) -> Value {
    let key = prop_key(rt, name).unwrap();
    rt.get(obj, key).unwrap()
  }

  fn value_to_string(rt: &VmJsRuntime, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value");
    };
    rt.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn device_pixel_ratio_matches_media_context() {
    let mut rt = VmJsRuntime::new();
    let window = rt.alloc_object_value().unwrap();
    let media = MediaContext::screen(800.0, 600.0).with_device_pixel_ratio(2.0);
    install_window_shims(&mut rt, window, WindowEnv::from_media(media)).unwrap();

    let dpr = get_prop(&mut rt, window, "devicePixelRatio");
    assert!(matches!(dpr, Value::Number(v) if (v - 2.0).abs() < f64::EPSILON));
  }

  #[test]
  fn navigator_user_agent_exists_and_is_stable() {
    let mut rt = VmJsRuntime::new();
    let window = rt.alloc_object_value().unwrap();
    let media = MediaContext::screen(800.0, 600.0);
    install_window_shims(&mut rt, window, WindowEnv::from_media(media)).unwrap();

    let navigator = get_prop(&mut rt, window, "navigator");
    let ua = get_prop(&mut rt, navigator, "userAgent");
    assert_eq!(value_to_string(&rt, ua), FASTRENDER_USER_AGENT);

    let platform = get_prop(&mut rt, navigator, "platform");
    assert_eq!(value_to_string(&rt, platform), "Win32");

    let on_line = get_prop(&mut rt, navigator, "onLine");
    assert_eq!(on_line, Value::Bool(true));

    let cookie_enabled = get_prop(&mut rt, navigator, "cookieEnabled");
    assert_eq!(cookie_enabled, Value::Bool(true));

    let concurrency = get_prop(&mut rt, navigator, "hardwareConcurrency");
    assert_eq!(concurrency, Value::Number(4.0));

    let device_memory = get_prop(&mut rt, navigator, "deviceMemory");
    assert_eq!(device_memory, Value::Number(8.0));

    let ua_data = get_prop(&mut rt, navigator, "userAgentData");
    assert!(matches!(ua_data, Value::Object(_)));
  }

  #[test]
  fn match_media_evaluates_width_and_resolution_queries() {
    let mut rt = VmJsRuntime::new();
    let window = rt.alloc_object_value().unwrap();
    let media = MediaContext::screen(800.0, 600.0).with_device_pixel_ratio(2.0);
    install_window_shims(&mut rt, window, WindowEnv::from_media(media)).unwrap();

    let match_media_fn = get_prop(&mut rt, window, "matchMedia");

    let query = rt.alloc_string_value("(min-width: 700px)").unwrap();
    let mql = rt.call_function(match_media_fn, window, &[query]).unwrap();
    let matches = get_prop(&mut rt, mql, "matches");
    assert!(matches == Value::Bool(true));

    let query = rt.alloc_string_value("(min-resolution: 2dppx)").unwrap();
    let mql = rt.call_function(match_media_fn, window, &[query]).unwrap();
    let matches = get_prop(&mut rt, mql, "matches");
    assert!(matches == Value::Bool(true));

    let query = rt.alloc_string_value("(max-resolution: 1.5dppx)").unwrap();
    let mql = rt.call_function(match_media_fn, window, &[query]).unwrap();
    let matches = get_prop(&mut rt, mql, "matches");
    assert!(matches == Value::Bool(false));
  }

  #[test]
  fn match_media_add_event_listener_fires_on_media_context_update() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/").unwrap();
    host
      .exec_script(
        r#"
        globalThis.__calls = 0;
        globalThis.__last = null;
        const mql = matchMedia("(min-width: 700px)");
        mql.addEventListener("change", e => { globalThis.__calls++; globalThis.__last = e.matches; });
        "#,
      )
      .unwrap();
    assert_eq!(
      host.exec_script(r#"matchMedia("(min-width: 700px)").matches"#).unwrap(),
      Value::Bool(true)
    );

    host
      .set_media_context(MediaContext::screen(600.0, 600.0))
      .unwrap();

    assert_eq!(
      host
        .run_until_idle(RunLimits {
          max_tasks: 10,
          max_microtasks: 100,
          max_wall_time: Some(Duration::from_secs(5)),
        })
        .unwrap(),
      RunUntilIdleOutcome::Idle
    );

    assert_eq!(host.exec_script("globalThis.__calls").unwrap(), Value::Number(1.0));
    assert_eq!(host.exec_script("globalThis.__last").unwrap(), Value::Bool(false));
  }

  #[test]
  fn match_media_add_listener_fires_on_media_context_update() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/").unwrap();
    host
      .exec_script(
        r#"
        globalThis.__calls = 0;
        globalThis.__last = null;
        const mql = matchMedia("(max-width: 700px)");
        mql.addListener(e => { globalThis.__calls++; globalThis.__last = e.matches; });
        "#,
      )
      .unwrap();
    assert_eq!(
      host.exec_script(r#"matchMedia("(max-width: 700px)").matches"#).unwrap(),
      Value::Bool(false)
    );

    host
      .set_media_context(MediaContext::screen(600.0, 600.0))
      .unwrap();

    assert_eq!(
      host
        .run_until_idle(RunLimits {
          max_tasks: 10,
          max_microtasks: 100,
          max_wall_time: Some(Duration::from_secs(5)),
        })
        .unwrap(),
      RunUntilIdleOutcome::Idle
    );

    assert_eq!(host.exec_script("globalThis.__calls").unwrap(), Value::Number(1.0));
    assert_eq!(host.exec_script("globalThis.__last").unwrap(), Value::Bool(true));
  }

  #[test]
  fn match_media_onchange_is_invoked() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/").unwrap();
    host
      .exec_script(
        r#"
        globalThis.__calls = 0;
        globalThis.__last = null;
        const mql = matchMedia("(min-width: 700px)");
        mql.onchange = e => { globalThis.__calls++; globalThis.__last = e.matches; };
        "#,
      )
      .unwrap();

    host
      .set_media_context(MediaContext::screen(600.0, 600.0))
      .unwrap();

    assert_eq!(
      host
        .run_until_idle(RunLimits {
          max_tasks: 10,
          max_microtasks: 100,
          max_wall_time: Some(Duration::from_secs(5)),
        })
        .unwrap(),
      RunUntilIdleOutcome::Idle
    );

    assert_eq!(host.exec_script("globalThis.__calls").unwrap(), Value::Number(1.0));
    assert_eq!(host.exec_script("globalThis.__last").unwrap(), Value::Bool(false));
  }

  #[test]
  fn match_media_overlong_query_is_truncated_and_non_throwing() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/").unwrap();
    host
      .exec_script(&format!(
        r#"
        globalThis.__mql = matchMedia("a".repeat({}));
        globalThis.__len = globalThis.__mql.media.length;
        globalThis.__matches = globalThis.__mql.matches;
        "#,
        MAX_MATCH_MEDIA_QUERY_CODE_UNITS + 16
      ))
      .unwrap();

    assert_eq!(
      host.exec_script("globalThis.__len").unwrap(),
      Value::Number(MAX_MATCH_MEDIA_QUERY_CODE_UNITS as f64)
    );
    assert_eq!(
      host.exec_script("globalThis.__matches").unwrap(),
      Value::Bool(false)
    );
  }

  #[test]
  fn navigator_send_beacon_exists_and_is_non_throwing() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/").unwrap();

    let is_function = host
      .exec_script("typeof navigator.sendBeacon === 'function'")
      .unwrap();
    assert_eq!(is_function, Value::Bool(true));

    let with_payload = host
      .exec_script(r#"navigator.sendBeacon("https://example.invalid/beacon", '{"a":1}')"#)
      .unwrap();
    assert_eq!(with_payload, Value::Bool(true));

    let without_payload = host
      .exec_script(r#"navigator.sendBeacon("https://example.invalid/beacon")"#)
      .unwrap();
    assert_eq!(without_payload, Value::Bool(true));

    // Common real-world pattern: pass a URL object (stringifies via `URL.prototype.toString()`).
    let with_url_object = host
      .exec_script(r#"navigator.sendBeacon(new URL("https://example.invalid/beacon"), '{"a":1}')"#)
      .unwrap();
    assert_eq!(with_url_object, Value::Bool(true));

    // Missing URL should be non-throwing and return `false`.
    let missing_url = host.exec_script("navigator.sendBeacon()").unwrap();
    assert_eq!(missing_url, Value::Bool(false));

    // Overlong URL strings should be rejected deterministically.
    let overlong_url = host
      .exec_script(&format!(
        r#"navigator.sendBeacon("a".repeat({}))"#,
        MAX_SEND_BEACON_URL_CODE_UNITS + 1
      ))
      .unwrap();
    assert_eq!(overlong_url, Value::Bool(false));

    // URL-ish objects whose `toString` throws must not cause `sendBeacon` to throw.
    let throwing_url_to_string = host
      .exec_script(
        r#"
        const u = new URL("https://example.invalid/beacon");
        u.toString = () => { throw new Error("nope"); };
        navigator.sendBeacon(u);
        "#,
      )
      .unwrap();
    assert_eq!(throwing_url_to_string, Value::Bool(false));

    let throwing_to_string = host
      .exec_script(
        r#"navigator.sendBeacon({ toString() { throw new Error("nope"); } })"#,
      )
      .unwrap();
    assert_eq!(throwing_to_string, Value::Bool(false));
  }

  #[test]
  fn navigator_user_agent_data_is_present_and_resolves_high_entropy_values() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/").unwrap();

    assert_eq!(
      host
        .exec_script("typeof navigator.userAgentData === 'object' && navigator.userAgentData !== null")
        .unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("Array.isArray(navigator.userAgentData.brands) && navigator.userAgentData.brands.length > 0")
        .unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("typeof navigator.userAgentData.brands[0].brand === 'string' && typeof navigator.userAgentData.brands[0].version === 'string'")
        .unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("typeof navigator.userAgentData.getHighEntropyValues === 'function'")
        .unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("typeof navigator.userAgentData.platform === 'string'")
        .unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("navigator.userAgentData.platform === 'Windows'")
        .unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("typeof navigator.userAgentData.mobile === 'boolean'")
        .unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("navigator.userAgentData.getHighEntropyValues([]) instanceof Promise")
        .unwrap(),
      Value::Bool(true)
    );

    host
      .exec_script(
        r#"
        globalThis.__ua_ch_done = false;
        globalThis.__ua_ch_list = null;
        globalThis.__ua_ch_brands = null;
        globalThis.__ua_ch_platform = null;
        globalThis.__ua_ch_platform_version = null;
        globalThis.__ua_ch_architecture = null;
        globalThis.__ua_ch_bitness = null;
        globalThis.__ua_ch_model = null;
        globalThis.__ua_ch_ua_full_version = null;
        globalThis.__ua_ch_mobile = null;
        globalThis.__ua_ch_form_factors = null;
        (async () => {
          const r = await navigator.userAgentData.getHighEntropyValues([
            "fullVersionList",
            "brands",
            "platform",
            "platformVersion",
            "architecture",
            "bitness",
            "model",
            "uaFullVersion",
            "mobile",
            "formFactors",
          ]);
          globalThis.__ua_ch_list = r.fullVersionList;
          globalThis.__ua_ch_brands = r.brands;
          globalThis.__ua_ch_platform = r.platform;
          globalThis.__ua_ch_platform_version = r.platformVersion;
          globalThis.__ua_ch_architecture = r.architecture;
          globalThis.__ua_ch_bitness = r.bitness;
          globalThis.__ua_ch_model = r.model;
          globalThis.__ua_ch_ua_full_version = r.uaFullVersion;
          globalThis.__ua_ch_mobile = r.mobile;
          globalThis.__ua_ch_form_factors = r.formFactors;
          globalThis.__ua_ch_done = true;
        })();
        "#,
      )
      .unwrap();
    host.perform_microtask_checkpoint().unwrap();

    assert_eq!(host.exec_script("globalThis.__ua_ch_done").unwrap(), Value::Bool(true));
    assert_eq!(
      host.exec_script("Array.isArray(globalThis.__ua_ch_list)").unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("globalThis.__ua_ch_list.length > 0 && typeof globalThis.__ua_ch_list[0].brand === 'string' && typeof globalThis.__ua_ch_list[0].version === 'string'")
        .unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host.exec_script("Array.isArray(globalThis.__ua_ch_brands)").unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("typeof globalThis.__ua_ch_platform === 'string' && globalThis.__ua_ch_platform === 'Windows'")
        .unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("typeof globalThis.__ua_ch_platform_version === 'string'")
        .unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("typeof globalThis.__ua_ch_architecture === 'string'")
        .unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host.exec_script("typeof globalThis.__ua_ch_bitness === 'string'").unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host.exec_script("typeof globalThis.__ua_ch_model === 'string'").unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("typeof globalThis.__ua_ch_ua_full_version === 'string'")
        .unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host.exec_script("typeof globalThis.__ua_ch_mobile === 'boolean'").unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("Array.isArray(globalThis.__ua_ch_form_factors)")
        .unwrap(),
      Value::Bool(true)
    );

    // UAData is a platform object in Chromium: structuredClone must reject it.
    assert_eq!(
      host.exec_script("typeof structuredClone === 'function'").unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script(
          "try { structuredClone(navigator.userAgentData); false } catch (e) { e && e.name === 'DataCloneError' }",
        )
        .unwrap(),
      Value::Bool(true)
    );

  }

  #[test]
  fn navigator_online_and_device_hints_are_present() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/").unwrap();

    assert_eq!(host.exec_script("navigator.onLine").unwrap(), Value::Bool(true));
    assert_eq!(
      host.exec_script("navigator.cookieEnabled").unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host.exec_script("navigator.hardwareConcurrency").unwrap(),
      Value::Number(4.0)
    );
    assert_eq!(
      host.exec_script("navigator.deviceMemory").unwrap(),
      Value::Number(8.0)
    );

    // Ensure the fields are read-only in sloppy mode: assignments should not stick.
    assert_eq!(
      host.exec_script("navigator.onLine = false; navigator.onLine").unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      host
        .exec_script("navigator.hardwareConcurrency = 1; navigator.hardwareConcurrency")
        .unwrap(),
      Value::Number(4.0)
    );
  }

  #[test]
  fn navigator_languages_is_array_and_supports_includes() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/").unwrap();

    let is_array = host.exec_script("Array.isArray(navigator.languages)").unwrap();
    assert_eq!(is_array, Value::Bool(true));

    let includes_en = host.exec_script("navigator.languages.includes('en')").unwrap();
    assert_eq!(includes_en, Value::Bool(true));
  }
}
