//! Privileged `chrome.*` API bindings for trusted chrome-frame pages.
//!
//! This module exposes a minimal JS→Rust command bridge that is **opt-in** per realm: embedders
//! must explicitly call [`install_chrome_api_bindings_vm_js`] for the target realm. Untrusted
//! content pages should never have this API installed.
//!
//! The installed API surface is intentionally hardened so trusted chrome UI code cannot
//! accidentally clobber it:
//! - `chrome`, `chrome.navigation`, and `chrome.tabs` are installed as non-writable,
//!   non-configurable properties.
//! - Methods are installed as non-writable, non-configurable data properties.
//! - The API objects are made non-extensible (best-effort).
//!
//! # Tab id representation
//!
//! Rust tab ids are `u64`, but JavaScript numbers are IEEE-754 doubles. To avoid silent precision
//! loss, this bridge accepts tab ids only as finite non-negative integers within
//! `Number.MAX_SAFE_INTEGER` (2^53 - 1).

use crate::js::WindowRealmHost;
use vm_js::{
  GcObject, GcString, Heap, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm,
  VmError, VmHost, VmHostHooks,
};
use webidl_vm_js::VmJsHostHooksPayload;

/// Hard upper bound on URL argument length accepted by the chrome API, measured in UTF-16 code
/// units.
///
/// This is a DoS-resistance measure: we must not allocate arbitrarily large host strings from JS
/// input.
pub const MAX_CHROME_API_URL_CODE_UNITS: usize = 8192;

const MAX_CHROME_API_TAB_ID_SAFE_INTEGER: u64 = (1u64 << 53) - 1;

const CHROME_API_HOST_NOT_AVAILABLE: &str =
  "chrome API host not available (missing VmJsHostHooksPayload embedder state)";

const NAVIGATE_ARG_COUNT_ERROR: &str = "chrome.navigation.navigate requires exactly 1 argument";
const NAVIGATE_URL_TYPE_ERROR: &str = "chrome.navigation.navigate requires a string URL";
const NAVIGATE_URL_TOO_LONG_ERROR: &str = "chrome.navigation.navigate URL exceeded max length";

const BACK_ARG_COUNT_ERROR: &str = "chrome.navigation.back requires 0 arguments";
const FORWARD_ARG_COUNT_ERROR: &str = "chrome.navigation.forward requires 0 arguments";
const RELOAD_ARG_COUNT_ERROR: &str = "chrome.navigation.reload requires 0 arguments";
const STOP_ARG_COUNT_ERROR: &str = "chrome.navigation.stop requires 0 arguments";

const NEW_TAB_ARG_COUNT_ERROR: &str = "chrome.tabs.newTab expects 0 or 1 arguments";
const NEW_TAB_URL_TYPE_ERROR: &str = "chrome.tabs.newTab URL must be a string when provided";
const NEW_TAB_URL_TOO_LONG_ERROR: &str = "chrome.tabs.newTab URL exceeded max length";

const CLOSE_TAB_ARG_COUNT_ERROR: &str = "chrome.tabs.closeTab requires exactly 1 argument";
const CLOSE_TAB_ID_TYPE_ERROR: &str = "chrome.tabs.closeTab requires a numeric tab id";
const CLOSE_TAB_ID_RANGE_ERROR: &str = "chrome.tabs.closeTab tab id exceeded Number.MAX_SAFE_INTEGER";

const ACTIVATE_TAB_ARG_COUNT_ERROR: &str = "chrome.tabs.activateTab requires exactly 1 argument";
const ACTIVATE_TAB_ID_TYPE_ERROR: &str = "chrome.tabs.activateTab requires a numeric tab id";
const ACTIVATE_TAB_ID_RANGE_ERROR: &str =
  "chrome.tabs.activateTab tab id exceeded Number.MAX_SAFE_INTEGER";

/// Chrome-frame command emitted by privileged JS bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeCommand {
  Navigate { url: String },
  Back,
  Forward,
  Reload,
  Stop,
  NewTab { url: Option<String> },
  CloseTab { tab_id: u64 },
  ActivateTab { tab_id: u64 },
}

/// Host integration surface for privileged chrome-frame APIs.
///
/// Embedders should install the JS bindings into trusted realms via
/// [`install_chrome_api_bindings_vm_js`], then implement this trait on their host state to receive
/// commands.
pub trait ChromeApiHost {
  fn chrome_dispatch(&mut self, cmd: ChromeCommand) -> Result<(), crate::error::Error>;
}

fn hooks_payload_mut<'a>(hooks: &'a mut dyn VmHostHooks) -> Option<&'a mut VmJsHostHooksPayload> {
  let any = hooks.as_any_mut()?;
  any.downcast_mut::<VmJsHostHooksPayload>()
}

fn chrome_api_host_mut<Host: ChromeApiHost + WindowRealmHost + 'static>(
  hooks: &mut dyn VmHostHooks,
) -> Result<&mut Host, VmError> {
  let Some(payload) = hooks_payload_mut(hooks) else {
    return Err(VmError::TypeError(CHROME_API_HOST_NOT_AVAILABLE));
  };
  payload
    .embedder_state_mut::<Host>()
    .ok_or(VmError::TypeError(CHROME_API_HOST_NOT_AVAILABLE))
}

fn throw_error(scope: &mut Scope<'_>, message: &str) -> VmError {
  match scope.alloc_string(message) {
    Ok(s) => VmError::Throw(Value::String(s)),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn dispatch_cmd<Host: ChromeApiHost + WindowRealmHost + 'static>(
  scope: &mut Scope<'_>,
  hooks: &mut dyn VmHostHooks,
  cmd: ChromeCommand,
) -> Result<(), VmError> {
  let host = chrome_api_host_mut::<Host>(hooks)?;
  host
    .chrome_dispatch(cmd)
    .map_err(|err| throw_error(scope, &err.to_string()))
}

fn js_string_to_rust_string_limited(
  scope: &Scope<'_>,
  handle: GcString,
  max_code_units: usize,
  too_long_error: &'static str,
) -> Result<String, VmError> {
  let js = scope.heap().get_string(handle)?;
  if js.as_code_units().len() > max_code_units {
    return Err(VmError::TypeError(too_long_error));
  }
  Ok(js.to_utf8_lossy())
}

fn number_to_tab_id(n: f64) -> Option<u64> {
  if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
    return None;
  }
  if n > (MAX_CHROME_API_TAB_ID_SAFE_INTEGER as f64) {
    return None;
  }
  // Defensive roundtrip check (rejects -0.0 and other oddities).
  let raw = n as u64;
  if raw as f64 != n {
    return None;
  }
  Some(raw)
}

fn chrome_navigation_navigate_native<Host: ChromeApiHost + WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _vm_host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if args.len() != 1 {
    return Err(VmError::TypeError(NAVIGATE_ARG_COUNT_ERROR));
  }
  let Value::String(url_s) = args[0] else {
    return Err(VmError::TypeError(NAVIGATE_URL_TYPE_ERROR));
  };
  let url = js_string_to_rust_string_limited(
    scope,
    url_s,
    MAX_CHROME_API_URL_CODE_UNITS,
    NAVIGATE_URL_TOO_LONG_ERROR,
  )?;
  dispatch_cmd::<Host>(scope, hooks, ChromeCommand::Navigate { url })?;
  Ok(Value::Undefined)
}

fn chrome_navigation_back_native<Host: ChromeApiHost + WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _vm_host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if !args.is_empty() {
    return Err(VmError::TypeError(BACK_ARG_COUNT_ERROR));
  }
  dispatch_cmd::<Host>(scope, hooks, ChromeCommand::Back)?;
  Ok(Value::Undefined)
}

fn chrome_navigation_forward_native<Host: ChromeApiHost + WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _vm_host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if !args.is_empty() {
    return Err(VmError::TypeError(FORWARD_ARG_COUNT_ERROR));
  }
  dispatch_cmd::<Host>(scope, hooks, ChromeCommand::Forward)?;
  Ok(Value::Undefined)
}

fn chrome_navigation_reload_native<Host: ChromeApiHost + WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _vm_host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if !args.is_empty() {
    return Err(VmError::TypeError(RELOAD_ARG_COUNT_ERROR));
  }
  dispatch_cmd::<Host>(scope, hooks, ChromeCommand::Reload)?;
  Ok(Value::Undefined)
}

fn chrome_navigation_stop_native<Host: ChromeApiHost + WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _vm_host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if !args.is_empty() {
    return Err(VmError::TypeError(STOP_ARG_COUNT_ERROR));
  }
  dispatch_cmd::<Host>(scope, hooks, ChromeCommand::Stop)?;
  Ok(Value::Undefined)
}

fn chrome_tabs_new_tab_native<Host: ChromeApiHost + WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _vm_host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if args.len() > 1 {
    return Err(VmError::TypeError(NEW_TAB_ARG_COUNT_ERROR));
  }
  let url = match args.get(0).copied().unwrap_or(Value::Undefined) {
    Value::Undefined | Value::Null => None,
    Value::String(s) => Some(js_string_to_rust_string_limited(
      scope,
      s,
      MAX_CHROME_API_URL_CODE_UNITS,
      NEW_TAB_URL_TOO_LONG_ERROR,
    )?),
    _ => return Err(VmError::TypeError(NEW_TAB_URL_TYPE_ERROR)),
  };
  dispatch_cmd::<Host>(scope, hooks, ChromeCommand::NewTab { url })?;
  Ok(Value::Undefined)
}

fn chrome_tabs_close_tab_native<Host: ChromeApiHost + WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _vm_host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if args.len() != 1 {
    return Err(VmError::TypeError(CLOSE_TAB_ARG_COUNT_ERROR));
  }
  let Value::Number(n) = args[0] else {
    return Err(VmError::TypeError(CLOSE_TAB_ID_TYPE_ERROR));
  };
  let Some(tab_id) = number_to_tab_id(n) else {
    if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n > (MAX_CHROME_API_TAB_ID_SAFE_INTEGER as f64)
    {
      return Err(VmError::TypeError(CLOSE_TAB_ID_RANGE_ERROR));
    }
    return Err(VmError::TypeError(CLOSE_TAB_ID_TYPE_ERROR));
  };
  dispatch_cmd::<Host>(scope, hooks, ChromeCommand::CloseTab { tab_id })?;
  Ok(Value::Undefined)
}

fn chrome_tabs_activate_tab_native<Host: ChromeApiHost + WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _vm_host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if args.len() != 1 {
    return Err(VmError::TypeError(ACTIVATE_TAB_ARG_COUNT_ERROR));
  }
  let Value::Number(n) = args[0] else {
    return Err(VmError::TypeError(ACTIVATE_TAB_ID_TYPE_ERROR));
  };
  let Some(tab_id) = number_to_tab_id(n) else {
    if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n > (MAX_CHROME_API_TAB_ID_SAFE_INTEGER as f64)
    {
      return Err(VmError::TypeError(ACTIVATE_TAB_ID_RANGE_ERROR));
    }
    return Err(VmError::TypeError(ACTIVATE_TAB_ID_TYPE_ERROR));
  };
  dispatch_cmd::<Host>(scope, hooks, ChromeCommand::ActivateTab { tab_id })?;
  Ok(Value::Undefined)
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn non_configurable_read_only_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: false,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
}

fn define_non_configurable_read_only(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
) -> Result<(), VmError> {
  // Root `obj` and `value` while allocating the property key: `alloc_key` can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, non_configurable_read_only_data_desc(value))
}

/// Install privileged `chrome.*` bindings into the provided `vm-js` realm.
///
/// This is **opt-in**: embedders should call this only for trusted chrome-frame realms. The
/// installer is non-clobbering: if `globalThis.chrome` already exists, this returns `Ok(())`
/// without modifying it.
pub fn install_chrome_api_bindings_vm_js<Host>(
  vm: &mut Vm,
  heap: &mut Heap,
  realm: &Realm,
) -> Result<(), VmError>
where
  Host: ChromeApiHost + WindowRealmHost + 'static,
{
  let global = realm.global_object();
  let func_proto = realm.intrinsics().function_prototype();
  let obj_proto = realm.intrinsics().object_prototype();

  let mut scope = heap.scope();
  scope.push_root(Value::Object(global))?;

  // Non-clobbering: if `globalThis.chrome` already exists as an own property, leave it alone.
  let chrome_key = alloc_key(&mut scope, "chrome")?;
  if scope
    .heap()
    .object_get_own_property(global, &chrome_key)?
    .is_some()
  {
    return Ok(());
  }

  let chrome_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(chrome_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(chrome_obj, Some(obj_proto))?;

  let navigation_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(navigation_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(navigation_obj, Some(obj_proto))?;

  let tabs_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(tabs_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(tabs_obj, Some(obj_proto))?;

  define_non_configurable_read_only(
    &mut scope,
    chrome_obj,
    "navigation",
    Value::Object(navigation_obj),
  )?;
  define_non_configurable_read_only(&mut scope, chrome_obj, "tabs", Value::Object(tabs_obj))?;

  // --- chrome.navigation ---
  let nav_navigate_id = vm.register_native_call(chrome_navigation_navigate_native::<Host>)?;
  let nav_back_id = vm.register_native_call(chrome_navigation_back_native::<Host>)?;
  let nav_forward_id = vm.register_native_call(chrome_navigation_forward_native::<Host>)?;
  let nav_reload_id = vm.register_native_call(chrome_navigation_reload_native::<Host>)?;
  let nav_stop_id = vm.register_native_call(chrome_navigation_stop_native::<Host>)?;

  let nav_navigate_name = scope.alloc_string("navigate")?;
  scope.push_root(Value::String(nav_navigate_name))?;
  let nav_navigate_func = scope.alloc_native_function(nav_navigate_id, None, nav_navigate_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(nav_navigate_func, Some(func_proto))?;
  scope.push_root(Value::Object(nav_navigate_func))?;
  define_non_configurable_read_only(
    &mut scope,
    navigation_obj,
    "navigate",
    Value::Object(nav_navigate_func),
  )?;

  let nav_back_name = scope.alloc_string("back")?;
  scope.push_root(Value::String(nav_back_name))?;
  let nav_back_func = scope.alloc_native_function(nav_back_id, None, nav_back_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(nav_back_func, Some(func_proto))?;
  scope.push_root(Value::Object(nav_back_func))?;
  define_non_configurable_read_only(
    &mut scope,
    navigation_obj,
    "back",
    Value::Object(nav_back_func),
  )?;

  let nav_forward_name = scope.alloc_string("forward")?;
  scope.push_root(Value::String(nav_forward_name))?;
  let nav_forward_func = scope.alloc_native_function(nav_forward_id, None, nav_forward_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(nav_forward_func, Some(func_proto))?;
  scope.push_root(Value::Object(nav_forward_func))?;
  define_non_configurable_read_only(
    &mut scope,
    navigation_obj,
    "forward",
    Value::Object(nav_forward_func),
  )?;

  let nav_reload_name = scope.alloc_string("reload")?;
  scope.push_root(Value::String(nav_reload_name))?;
  let nav_reload_func = scope.alloc_native_function(nav_reload_id, None, nav_reload_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(nav_reload_func, Some(func_proto))?;
  scope.push_root(Value::Object(nav_reload_func))?;
  define_non_configurable_read_only(
    &mut scope,
    navigation_obj,
    "reload",
    Value::Object(nav_reload_func),
  )?;

  let nav_stop_name = scope.alloc_string("stop")?;
  scope.push_root(Value::String(nav_stop_name))?;
  let nav_stop_func = scope.alloc_native_function(nav_stop_id, None, nav_stop_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(nav_stop_func, Some(func_proto))?;
  scope.push_root(Value::Object(nav_stop_func))?;
  define_non_configurable_read_only(
    &mut scope,
    navigation_obj,
    "stop",
    Value::Object(nav_stop_func),
  )?;

  // --- chrome.tabs ---
  let tabs_new_tab_id = vm.register_native_call(chrome_tabs_new_tab_native::<Host>)?;
  let tabs_close_tab_id = vm.register_native_call(chrome_tabs_close_tab_native::<Host>)?;
  let tabs_activate_tab_id = vm.register_native_call(chrome_tabs_activate_tab_native::<Host>)?;

  let tabs_new_tab_name = scope.alloc_string("newTab")?;
  scope.push_root(Value::String(tabs_new_tab_name))?;
  let tabs_new_tab_func = scope.alloc_native_function(tabs_new_tab_id, None, tabs_new_tab_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(tabs_new_tab_func, Some(func_proto))?;
  scope.push_root(Value::Object(tabs_new_tab_func))?;
  define_non_configurable_read_only(&mut scope, tabs_obj, "newTab", Value::Object(tabs_new_tab_func))?;

  let tabs_close_tab_name = scope.alloc_string("closeTab")?;
  scope.push_root(Value::String(tabs_close_tab_name))?;
  let tabs_close_tab_func =
    scope.alloc_native_function(tabs_close_tab_id, None, tabs_close_tab_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(tabs_close_tab_func, Some(func_proto))?;
  scope.push_root(Value::Object(tabs_close_tab_func))?;
  define_non_configurable_read_only(
    &mut scope,
    tabs_obj,
    "closeTab",
    Value::Object(tabs_close_tab_func),
  )?;

  let tabs_activate_tab_name = scope.alloc_string("activateTab")?;
  scope.push_root(Value::String(tabs_activate_tab_name))?;
  let tabs_activate_tab_func =
    scope.alloc_native_function(tabs_activate_tab_id, None, tabs_activate_tab_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(tabs_activate_tab_func, Some(func_proto))?;
  scope.push_root(Value::Object(tabs_activate_tab_func))?;
  define_non_configurable_read_only(
    &mut scope,
    tabs_obj,
    "activateTab",
    Value::Object(tabs_activate_tab_func),
  )?;

  // Define `globalThis.chrome` as a non-configurable, non-writable data property.
  scope.define_property(
    global,
    chrome_key,
    non_configurable_read_only_data_desc(Value::Object(chrome_obj)),
  )?;

  // Best-effort hardening: the property descriptors above should still be installed even if these
  // operations fail (e.g. due to resource exhaustion).
  let _ = scope.object_prevent_extensions(chrome_obj);
  let _ = scope.object_prevent_extensions(navigation_obj);
  let _ = scope.object_prevent_extensions(tabs_obj);

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::window_timers::VmJsEventLoopHooks;
  use crate::js::{WindowRealm, WindowRealmConfig};
  use std::any::Any;
  use vm_js::{Job, VmHostHooks};

  struct TestHost {
    vm_host: (),
    realm: WindowRealm,
    cmds: Vec<ChromeCommand>,
  }

  impl TestHost {
    fn new() -> Self {
      Self {
        vm_host: (),
        realm: WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))
          .expect("create realm"),
        cmds: Vec::new(),
      }
    }
  }

  impl WindowRealmHost for TestHost {
    fn vm_host_and_window_realm(
      &mut self,
    ) -> crate::error::Result<(&mut dyn VmHost, &mut WindowRealm)> {
      Ok((&mut self.vm_host, &mut self.realm))
    }
  }

  impl ChromeApiHost for TestHost {
    fn chrome_dispatch(&mut self, cmd: ChromeCommand) -> Result<(), crate::error::Error> {
      self.cmds.push(cmd);
      Ok(())
    }
  }

  // Minimal hooks impl that exposes the VmJsEventLoopHooks payload without requiring an active
  // EventLoop.
  struct Hooks<Host: WindowRealmHost + 'static> {
    inner: VmJsEventLoopHooks<Host>,
  }

  impl<Host: WindowRealmHost + 'static> Hooks<Host> {
    fn new(host: &mut Host) -> crate::error::Result<Self> {
      Ok(Self {
        inner: VmJsEventLoopHooks::new_with_host(host)?,
      })
    }
  }

  impl<Host: WindowRealmHost + 'static> VmHostHooks for Hooks<Host> {
    fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<vm_js::RealmId>) {}

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
      vm_js::VmHostHooks::as_any_mut(&mut self.inner)
    }
  }

  #[test]
  fn installer_is_non_clobbering() {
    let mut host = TestHost::new();
    {
      // Define chrome first.
      let _ = host
        .realm
        .exec_script("globalThis.chrome = 123;")
        .expect("script should run");

      let (vm, realm, heap) = host.realm.vm_realm_and_heap_mut();
      install_chrome_api_bindings_vm_js::<TestHost>(vm, heap, realm).expect("install should succeed");
    }

    let v = host
      .realm
      .exec_script("globalThis.chrome")
      .expect("read chrome");
    assert_eq!(v, Value::Number(123.0));
  }

  #[test]
  fn dispatches_commands_via_embedder_state() {
    let mut host = TestHost::new();
    {
      let (vm, realm, heap) = host.realm.vm_realm_and_heap_mut();
      install_chrome_api_bindings_vm_js::<TestHost>(vm, heap, realm).expect("install should succeed");
    }

    let mut hooks = Hooks::<TestHost>::new(&mut host).expect("create hooks");
    let (vm_host, realm) = host.vm_host_and_window_realm().expect("split");
    let _ = realm
      .exec_script_with_host_and_hooks(vm_host, &mut hooks, "chrome.navigation.back();")
      .expect("script should run");

    assert_eq!(host.cmds, vec![ChromeCommand::Back]);
  }

  #[test]
  fn navigate_enforces_string_length_bound() {
    let mut host = TestHost::new();
    {
      let (vm, realm, heap) = host.realm.vm_realm_and_heap_mut();
      install_chrome_api_bindings_vm_js::<TestHost>(vm, heap, realm).expect("install should succeed");
    }

    let mut hooks = Hooks::<TestHost>::new(&mut host).expect("create hooks");
    let (vm_host, realm) = host.vm_host_and_window_realm().expect("split");

    // 8193 UTF-16 code units (ASCII => 1 code unit per char).
    let url = "a".repeat(MAX_CHROME_API_URL_CODE_UNITS + 1);
    let src = format!(
      r#"
      (() => {{
        try {{ chrome.navigation.navigate("{url}"); return "no-error"; }}
        catch (e) {{ return e && e.name || String(e); }}
      }})()
      "#
    );

    let v = realm
      .exec_script_with_host_and_hooks(vm_host, &mut hooks, &src)
      .expect("script should catch and return");
    // `VmError::TypeError` should be coerced into a real TypeError object.
    fn js_value_to_utf8(heap: &vm_js::Heap, value: Value) -> String {
      match value {
        Value::String(s) => heap
          .get_string(s)
          .map(|s| s.to_utf8_lossy())
          .unwrap_or_default(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Undefined => "undefined".to_string(),
        Value::Null => "null".to_string(),
        other => format!("{other:?}"),
      }
    }
    let got = js_value_to_utf8(realm.heap(), v);
    assert_eq!(got, "TypeError");
  }

  #[test]
  fn chrome_api_objects_are_hardened() {
    let mut host = TestHost::new();
    {
      let (vm, realm, heap) = host.realm.vm_realm_and_heap_mut();
      install_chrome_api_bindings_vm_js::<TestHost>(vm, heap, realm).expect("install should succeed");
    }

    let value = host
      .realm
      .exec_script(
        r#"
          (() => {
            'use strict';
            try { chrome.newProp = 1; return false; } catch (e) { return true; }
          })()
        "#,
      )
      .expect("script should run");
    assert_eq!(value, Value::Bool(true));

    let value = host
      .realm
      .exec_script(
        r#"
          (() => {
            'use strict';
            try { chrome.navigation.navigate = () => {}; return false; } catch (e) { return true; }
          })()
        "#,
      )
      .expect("script should run");
    assert_eq!(value, Value::Bool(true));

    let value = host
      .realm
      .exec_script("Object.isExtensible(chrome) === false")
      .expect("script should run");
    assert_eq!(value, Value::Bool(true));
  }
}
