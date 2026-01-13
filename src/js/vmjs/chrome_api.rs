//! Privileged chrome JS bridge for trusted UI pages.
//!
//! This module defines a minimal `globalThis.chrome` surface intended for *trusted* chrome/UI pages
//! (renderer chrome workstream). It must never be installed into untrusted content realms.
//!
//! For the privileged internal URL schemes reserved for those trusted chrome pages (`chrome://`
//! assets and `chrome-action:` actions), see `docs/renderer_chrome_schemes.md`.
//!
//! # Tab id representation
//!
//! Rust tab ids are `u64` (`crate::ui::messages::TabId(pub u64)`), but JS `Number` cannot precisely
//! represent all `u64` values. This module uses **Number safe integers** as the canonical JS
//! representation for tab ids:
//!
//! - `typeof id === "number"`
//! - `Number.isFinite(id) === true`
//! - `Number.isInteger(id) === true`
//! - `0 <= id <= 2^53 - 1` (`Number.MAX_SAFE_INTEGER`)
//!
//! Any value outside this range is rejected with a synchronous `TypeError` so the bridge never
//! silently loses precision.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use vm_js::{
  GcObject, Heap, HostSlots, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm,
  VmError, VmHost, VmHostHooks,
};

const MAX_SAFE_INTEGER_U64: u64 = (1u64 << 53) - 1;

// Brand chrome objects as platform objects.
const CHROME_HOST_TAG: u64 = u64::from_be_bytes(*b"FRCHROME");
const CHROME_TABS_HOST_TAG: u64 = u64::from_be_bytes(*b"FRCHTABS");

static NEXT_CHROME_ENV_ID: AtomicU64 = AtomicU64::new(1);
static CHROME_ENVS: OnceLock<Mutex<HashMap<u64, ChromeEnvState>>> = OnceLock::new();

fn chrome_envs() -> &'static Mutex<HashMap<u64, ChromeEnvState>> {
  CHROME_ENVS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_chrome_env(env: ChromeEnvState) -> u64 {
  let env_id = NEXT_CHROME_ENV_ID.fetch_add(1, Ordering::Relaxed);
  let mut lock = chrome_envs()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  lock.insert(env_id, env);
  env_id
}

pub fn unregister_chrome_env(env_id: u64) {
  let mut lock = chrome_envs()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  lock.remove(&env_id);
}

fn with_env<R>(env_id: u64, f: impl FnOnce(&ChromeEnvState) -> R) -> Result<R, VmError> {
  let lock = chrome_envs()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  let env = lock
    .get(&env_id)
    .ok_or(VmError::TypeError("chrome env is not available"))?;
  Ok(f(env))
}

/// A chrome tabs snapshot entry returned by `chrome.tabs.getAll()`.
#[derive(Debug, Clone)]
pub struct ChromeTabInfo {
  pub id: u64,
  pub url: String,
  pub title: String,
  pub active: bool,
}

/// Host callbacks used by the chrome JS bridge.
///
/// This intentionally uses `u64` for ids: callers that have a `TabId` newtype can wrap/unwrap at
/// the boundary.
pub trait ChromeApiHandler: Send + Sync + 'static {
  fn new_tab(&self, url: Option<String>) -> u64;
  fn close_tab(&self, id: u64);
  fn activate_tab(&self, id: u64);
  fn tabs_snapshot(&self) -> Vec<ChromeTabInfo>;
}

#[derive(Clone)]
struct ChromeEnvState {
  handler: Arc<dyn ChromeApiHandler>,
}

/// RAII guard returned by [`install_chrome_api_bindings_vm_js`].
///
/// Dropping this guard unregisters the backing Rust env state for the chrome JS bridge installed
/// into a realm.
#[derive(Debug)]
#[must_use = "chrome api bindings are only valid while the returned ChromeApiBindings is kept alive"]
pub struct ChromeApiBindings {
  env_id: u64,
  active: bool,
}

impl ChromeApiBindings {
  fn new(env_id: u64) -> Self {
    Self {
      env_id,
      active: true,
    }
  }

  /// Disable automatic cleanup and return the env id.
  #[allow(dead_code)]
  fn disarm(mut self) -> u64 {
    self.active = false;
    self.env_id
  }

  #[allow(dead_code)]
  pub fn env_id(&self) -> u64 {
    self.env_id
  }
}

impl Drop for ChromeApiBindings {
  fn drop(&mut self) {
    if self.active {
      unregister_chrome_env(self.env_id);
    }
  }
}

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

fn readonly_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
}

fn snapshot_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn env_id_from_tabs_this(scope: &Scope<'_>, this: Value) -> Result<u64, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let slots = scope
    .heap()
    .object_host_slots(obj)?
    .ok_or(VmError::TypeError("Illegal invocation"))?;
  if slots.a != CHROME_TABS_HOST_TAG {
    return Err(VmError::TypeError("Illegal invocation"));
  }
  Ok(slots.b)
}

/// Convert a JS tab id value into a `u64` without risking `Number` precision loss.
pub fn tab_id_from_js(scope: &Scope<'_>, value: Value) -> Result<u64, VmError> {
  let _ = scope;
  let Value::Number(n) = value else {
    return Err(VmError::TypeError("tab id must be a Number"));
  };
  if !n.is_finite() {
    return Err(VmError::TypeError("tab id must be a finite Number"));
  }
  if n < 0.0 {
    return Err(VmError::TypeError("tab id must be a non-negative integer"));
  }
  if n.fract() != 0.0 {
    return Err(VmError::TypeError("tab id must be an integer"));
  }
  if n > (MAX_SAFE_INTEGER_U64 as f64) {
    return Err(VmError::TypeError(
      "tab id exceeds Number.MAX_SAFE_INTEGER",
    ));
  }
  let id = n as u64;
  // Ensure there is no loss even within range (defensive: catches -0.0 etc).
  if (id as f64) != n {
    return Err(VmError::TypeError("tab id is not representable as a safe integer"));
  }
  Ok(id)
}

/// Convert a host `u64` tab id into a JS value without risking `Number` precision loss.
pub fn tab_id_to_js(_scope: &mut Scope<'_>, id: u64) -> Result<Value, VmError> {
  if id > MAX_SAFE_INTEGER_U64 {
    return Err(VmError::TypeError(
      "tab id exceeds Number.MAX_SAFE_INTEGER",
    ));
  }
  Ok(Value::Number(id as f64))
}

fn chrome_tabs_close_tab_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let env_id = env_id_from_tabs_this(scope, this)?;
  let id_v = args.get(0).copied().unwrap_or(Value::Undefined);
  let id = tab_id_from_js(scope, id_v)?;
  with_env(env_id, |env| env.handler.close_tab(id))?;
  Ok(Value::Undefined)
}

fn chrome_tabs_activate_tab_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let env_id = env_id_from_tabs_this(scope, this)?;
  let id_v = args.get(0).copied().unwrap_or(Value::Undefined);
  let id = tab_id_from_js(scope, id_v)?;
  with_env(env_id, |env| env.handler.activate_tab(id))?;
  Ok(Value::Undefined)
}

fn chrome_tabs_new_tab_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let env_id = env_id_from_tabs_this(scope, this)?;
  let url = match args.get(0).copied().unwrap_or(Value::Undefined) {
    Value::Undefined => None,
    Value::String(s) => Some(scope.heap().get_string(s)?.to_utf8_lossy()),
    _ => return Err(VmError::TypeError("url must be a string")),
  };
  let id = with_env(env_id, |env| env.handler.new_tab(url))?;
  tab_id_to_js(scope, id)
}

fn chrome_tabs_get_all_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let env_id = env_id_from_tabs_this(scope, this)?;
  let tabs = with_env(env_id, |env| env.handler.tabs_snapshot())?;
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "chrome.tabs.getAll requires intrinsics (create a Realm first)",
  ))?;

  let arr = scope.alloc_array(tabs.len())?;
  scope.push_root(Value::Object(arr))?;
  scope
    .heap_mut()
    .object_set_prototype(arr, Some(intr.array_prototype()))?;

  let id_key = alloc_key(scope, "id")?;
  let url_key = alloc_key(scope, "url")?;
  let title_key = alloc_key(scope, "title")?;
  let active_key = alloc_key(scope, "active")?;

  for (i, tab) in tabs.into_iter().enumerate() {
    let obj = scope.alloc_object()?;
    scope.push_root(Value::Object(obj))?;

    let id_v = tab_id_to_js(scope, tab.id)?;
    scope.define_property(obj, id_key, snapshot_data_desc(id_v))?;

    let url_s = scope.alloc_string(&tab.url)?;
    scope.push_root(Value::String(url_s))?;
    scope.define_property(obj, url_key, snapshot_data_desc(Value::String(url_s)))?;

    let title_s = scope.alloc_string(&tab.title)?;
    scope.push_root(Value::String(title_s))?;
    scope.define_property(
      obj,
      title_key,
      snapshot_data_desc(Value::String(title_s)),
    )?;

    scope.define_property(obj, active_key, snapshot_data_desc(Value::Bool(tab.active)))?;

    let idx_key = alloc_key(scope, &i.to_string())?;
    scope.ordinary_set(vm, arr, idx_key, Value::Object(obj), Value::Object(arr))?;
  }

  Ok(Value::Object(arr))
}

fn install_tabs_object(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  realm: &Realm,
  env_id: u64,
) -> Result<GcObject, VmError> {
  let tabs_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(tabs_obj))?;
  scope.heap_mut().object_set_host_slots(
    tabs_obj,
    HostSlots {
      a: CHROME_TABS_HOST_TAG,
      b: env_id,
    },
  )?;
  scope
    .heap_mut()
    .object_set_prototype(tabs_obj, Some(realm.intrinsics().object_prototype()))?;

  fn install_method(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    realm: &Realm,
    obj: GcObject,
    name: &str,
    call: vm_js::NativeCall,
    length: u32,
  ) -> Result<(), VmError> {
    let call_id = vm.register_native_call(call)?;
    let name_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(name_s))?;
    let func = scope.alloc_native_function(call_id, None, name_s, length)?;
    scope
      .heap_mut()
      .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(func))?;
    let key = alloc_key(scope, name)?;
    scope.define_property(obj, key, data_desc(Value::Object(func)))?;
    Ok(())
  }

  install_method(
    vm,
    scope,
    realm,
    tabs_obj,
    "newTab",
    chrome_tabs_new_tab_native,
    0,
  )?;
  install_method(
    vm,
    scope,
    realm,
    tabs_obj,
    "closeTab",
    chrome_tabs_close_tab_native,
    1,
  )?;
  install_method(
    vm,
    scope,
    realm,
    tabs_obj,
    "activateTab",
    chrome_tabs_activate_tab_native,
    1,
  )?;
  install_method(
    vm,
    scope,
    realm,
    tabs_obj,
    "getAll",
    chrome_tabs_get_all_native,
    0,
  )?;
  Ok(tabs_obj)
}

/// Installs `globalThis.chrome` into a `vm-js` realm.
///
/// Callers must ensure this is only used for *trusted* UI pages. Untrusted web content must never
/// have access to this API surface.
pub fn install_chrome_api_bindings_vm_js(
  vm: &mut Vm,
  heap: &mut Heap,
  realm: &Realm,
  handler: Arc<dyn ChromeApiHandler>,
) -> Result<ChromeApiBindings, VmError> {
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // Do not clobber an existing `chrome` global.
  let chrome_key = alloc_key(&mut scope, "chrome")?;
  if scope
    .heap()
    .object_get_own_property(global, &chrome_key)?
    .is_some()
  {
    // Keep semantics simple: treat as idempotent and do not install a second env.
    return Ok(ChromeApiBindings {
      env_id: 0,
      active: false,
    });
  }

  let env_id = register_chrome_env(ChromeEnvState { handler });

  let chrome_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(chrome_obj))?;
  scope.heap_mut().object_set_host_slots(
    chrome_obj,
    HostSlots {
      a: CHROME_HOST_TAG,
      b: env_id,
    },
  )?;
  scope
    .heap_mut()
    .object_set_prototype(chrome_obj, Some(realm.intrinsics().object_prototype()))?;

  let tabs_obj = install_tabs_object(vm, &mut scope, realm, env_id)?;
  let tabs_key = alloc_key(&mut scope, "tabs")?;
  scope.define_property(
    chrome_obj,
    tabs_key,
    readonly_data_desc(Value::Object(tabs_obj)),
  )?;

  scope.define_property(global, chrome_key, data_desc(Value::Object(chrome_obj)))?;

  Ok(ChromeApiBindings::new(env_id))
}
