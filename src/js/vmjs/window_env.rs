use crate::js::webidl::legacy::VmJsRuntime;
use crate::style::media::{MediaContext, MediaQuery};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use vm_js::{
  GcObject, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks,
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

const MATCH_MEDIA_SLOT_ENV_ID: usize = 0;
const MATCH_MEDIA_SLOT_NOOP_LISTENER: usize = 1;

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

fn match_media_envs() -> &'static Mutex<HashMap<u64, MediaContext>> {
  MATCH_MEDIA_ENVS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_match_media_env(media: MediaContext) -> u64 {
  let id = NEXT_MATCH_MEDIA_ENV_ID.fetch_add(1, Ordering::Relaxed);
  match_media_envs().lock().insert(id, media);
  id
}

pub(crate) fn unregister_match_media_env(id: u64) {
  match_media_envs().lock().remove(&id);
}

fn with_match_media_env<T>(id: u64, f: impl FnOnce(&MediaContext) -> T) -> Option<T> {
  let lock = match_media_envs().lock();
  let env = lock.get(&id)?;
  Some(f(env))
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
    return Err(VmError::Unimplemented("alloc_string_value returned non-string"));
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

fn noop_listener_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Undefined)
}

fn env_id_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<u64, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.get(MATCH_MEDIA_SLOT_ENV_ID).copied().unwrap_or(Value::Undefined) {
    Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 => Ok(n as u64),
    _ => Err(VmError::InvariantViolation("matchMedia missing env id native slot")),
  }
}

fn noop_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots
    .get(MATCH_MEDIA_SLOT_NOOP_LISTENER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => Ok(Value::Object(obj)),
    _ => Err(VmError::InvariantViolation(
      "matchMedia missing noop listener native slot",
    )),
  }
}

fn match_media_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  let env_id = env_id_from_callee(&scope, callee)?;
  let noop = noop_from_callee(&scope, callee)?;

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

  let (matches, media_value) = if too_long {
    let truncated = scope.alloc_string(&query_text)?;
    scope.push_root(Value::String(truncated))?;
    (false, Value::String(truncated))
  } else {
    let matches = MediaQuery::parse_list(&query_text)
      .ok()
      .is_some_and(|queries| with_match_media_env(env_id, |ctx| ctx.evaluate_list(&queries)).unwrap_or(false));
    (matches, Value::String(s))
  };

  let mql = scope.alloc_object()?;
  scope.push_root(Value::Object(mql))?;
  define_read_only_vm_js(&mut scope, mql, "matches", Value::Bool(matches))?;
  define_read_only_vm_js(&mut scope, mql, "media", media_value)?;
  define_read_only_vm_js(&mut scope, mql, "addListener", noop)?;
  define_read_only_vm_js(&mut scope, mql, "removeListener", noop)?;
  define_read_only_vm_js(&mut scope, mql, "addEventListener", noop)?;
  define_read_only_vm_js(&mut scope, mql, "removeEventListener", noop)?;

  Ok(Value::Object(mql))
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

  define_read_only_vm_js(scope, window, "devicePixelRatio", Value::Number(dpr))?;
  define_read_only_vm_js(scope, window, "innerWidth", Value::Number(viewport_width))?;
  define_read_only_vm_js(scope, window, "innerHeight", Value::Number(viewport_height))?;
  define_read_only_vm_js(scope, window, "outerWidth", Value::Number(viewport_width))?;
  define_read_only_vm_js(scope, window, "outerHeight", Value::Number(viewport_height))?;

  let screen = scope.alloc_object()?;
  scope.push_root(Value::Object(screen))?;
  define_read_only_vm_js(scope, screen, "width", Value::Number(device_width))?;
  define_read_only_vm_js(scope, screen, "height", Value::Number(device_height))?;
  define_read_only_vm_js(scope, screen, "availWidth", Value::Number(device_width))?;
  define_read_only_vm_js(scope, screen, "availHeight", Value::Number(device_height))?;
  define_read_only_vm_js(scope, window, "screen", Value::Object(screen))?;

  let navigator = scope.alloc_object()?;
  scope.push_root(Value::Object(navigator))?;
  let user_agent_s = scope.alloc_string(env.user_agent)?;
  scope.push_root(Value::String(user_agent_s))?;
  define_read_only_vm_js(scope, navigator, "userAgent", Value::String(user_agent_s))?;
  let platform_s = scope.alloc_string(env.platform)?;
  scope.push_root(Value::String(platform_s))?;
  define_read_only_vm_js(scope, navigator, "platform", Value::String(platform_s))?;
  let language_s = scope.alloc_string(env.language)?;
  scope.push_root(Value::String(language_s))?;
  define_read_only_vm_js(scope, navigator, "language", Value::String(language_s))?;

  let languages = scope.alloc_object()?;
  scope.push_root(Value::Object(languages))?;
  for (idx, lang) in env.languages.iter().enumerate() {
    let idx_key = alloc_key_vm_js(scope, &idx.to_string())?;
    let lang_s = scope.alloc_string(lang)?;
    scope.push_root(Value::String(lang_s))?;
    scope.define_property(languages, idx_key, read_only_data_desc(Value::String(lang_s)))?;
  }
  define_read_only_vm_js(scope, languages, "length", Value::Number(env.languages.len() as f64))?;
  define_read_only_vm_js(scope, navigator, "languages", Value::Object(languages))?;
  define_read_only_vm_js(scope, window, "navigator", Value::Object(navigator))?;

  let noop_call_id = vm.register_native_call(noop_listener_native)?;
  let noop_name = scope.alloc_string("matchMedia listener noop")?;
  scope.push_root(Value::String(noop_name))?;
  let noop_func = scope.alloc_native_function(noop_call_id, None, noop_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(noop_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(noop_func))?;

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
      Value::Object(noop_func),
    ],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(match_media_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(match_media_func))?;

  define_read_only_vm_js(scope, window, "matchMedia", Value::Object(match_media_func))?;

  Ok(())
}

/// Installs basic browser-environment shims onto a window-like global object.
///
/// The installed surface is intentionally minimal and deterministic:
/// - `window.devicePixelRatio`
/// - viewport geometry (`innerWidth`/`innerHeight`, `outerWidth`/`outerHeight`, `screen.*`)
/// - `navigator` (`userAgent`, `platform`, `language`, `languages`)
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

  // `navigator.languages` is an array in browsers; represent it as a tiny array-like object.
  let languages = rt.alloc_object_value()?;
  for (idx, lang) in env.languages.iter().enumerate() {
    let idx_key = prop_key(rt, &idx.to_string())?;
    let lang_value = rt.alloc_string_value(lang)?;
    rt.define_data_property(
      languages,
      idx_key,
      lang_value,
      true,
    )?;
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
  use crate::style::media::MediaContext;

  fn get_prop(rt: &mut VmJsRuntime, obj: Value, name: &str) -> Value {
    let key = prop_key(rt, name).unwrap();
    rt.get(obj, key).unwrap()
  }

  fn value_to_string(rt: &VmJsRuntime, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value");
    };
    rt
      .heap()
      .get_string(s)
      .unwrap()
      .to_utf8_lossy()
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
  }

  #[test]
  fn match_media_evaluates_width_and_resolution_queries() {
    let mut rt = VmJsRuntime::new();
    let window = rt.alloc_object_value().unwrap();
    let media = MediaContext::screen(800.0, 600.0).with_device_pixel_ratio(2.0);
    install_window_shims(&mut rt, window, WindowEnv::from_media(media)).unwrap();

    let match_media_fn = get_prop(&mut rt, window, "matchMedia");

    let query = rt.alloc_string_value("(min-width: 700px)").unwrap();
    let mql = rt
      .call_function(match_media_fn, window, &[query])
      .unwrap();
    let matches = get_prop(&mut rt, mql, "matches");
    assert!(matches == Value::Bool(true));

    let query = rt.alloc_string_value("(min-resolution: 2dppx)").unwrap();
    let mql = rt
      .call_function(match_media_fn, window, &[query])
      .unwrap();
    let matches = get_prop(&mut rt, mql, "matches");
    assert!(matches == Value::Bool(true));

    let query = rt.alloc_string_value("(max-resolution: 1.5dppx)").unwrap();
    let mql = rt
      .call_function(match_media_fn, window, &[query])
      .unwrap();
    let matches = get_prop(&mut rt, mql, "matches");
    assert!(matches == Value::Bool(false));
  }
}
