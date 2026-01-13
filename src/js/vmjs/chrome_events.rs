//! Host→JS event helpers for trusted chrome pages.
//!
//! These utilities allow the embedder/browser process to push state updates into the chrome page's
//! JavaScript realm without polling.

use crate::error::{Error, Result};
use crate::js::time::update_time_bindings_clock;
use crate::js::vm_error_format;
use crate::js::window_realm::WindowRealmHost;
use crate::js::window_timers::VmJsEventLoopHooks;
use crate::js::{EventLoop, TaskSource};
use vm_js::{
  GcObject, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks,
};

/// Upper bound on the UTF-8 byte length of a chrome event type.
///
/// Chrome event types are typically small ASCII strings (e.g. `"chrome-tabs"`).
const MAX_CHROME_EVENT_TYPE_BYTES: usize = 256;

/// Upper bound on the UTF-8 byte length of `detail_json`.
///
/// This is intentionally bounded even though chrome pages are trusted: it prevents accidental
/// unbounded memory growth if the embedder sends malformed/huge state snapshots.
const MAX_CHROME_EVENT_DETAIL_JSON_BYTES: usize = 1024 * 1024; // 1 MiB

type VmResult<T> = std::result::Result<T, VmError>;

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> VmResult<PropertyKey> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn dispatch_chrome_event_in_vm(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  vm_host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  window_obj: GcObject,
  event_type: &str,
  detail_json: &str,
) -> VmResult<()> {
  let mut scope = scope.reborrow();

  // Root `window_obj` so GC triggered by subsequent allocations cannot invalidate it.
  scope.push_root(Value::Object(window_obj))?;

  // Allocate `event_type` early since it is needed for `new CustomEvent(..)`.
  let event_type_s = scope.alloc_string(event_type)?;
  scope.push_root(Value::String(event_type_s))?;

  // Parse `detail_json` via `JSON.parse`. If parsing fails, dispatch with `detail: null` to keep
  // this helper forgiving for embedder bugs.
  let detail_value = if detail_json.is_empty() {
    Value::Null
  } else {
    let json_key = alloc_key(&mut scope, "JSON")?;
    let json_value = vm.get_with_host_and_hooks(vm_host, &mut scope, hooks, window_obj, json_key)?;
    let Value::Object(json_obj) = json_value else {
      return Err(VmError::TypeError("window.JSON is not an object"));
    };
    scope.push_root(Value::Object(json_obj))?;

    let parse_key = alloc_key(&mut scope, "parse")?;
    let parse_value = vm.get_with_host_and_hooks(vm_host, &mut scope, hooks, json_obj, parse_key)?;
    scope.push_root(parse_value)?;

    let detail_s = scope.alloc_string(detail_json)?;
    scope.push_root(Value::String(detail_s))?;

    match vm.call_with_host_and_hooks(
      vm_host,
      &mut scope,
      hooks,
      parse_value,
      Value::Object(json_obj),
      &[Value::String(detail_s)],
    ) {
      Ok(v) => v,
      Err(e @ VmError::Termination(_)) => return Err(e),
      Err(_) => Value::Null,
    }
  };
  scope.push_root(detail_value)?;

  let intr = vm
    .intrinsics()
    .ok_or(VmError::InvariantViolation("missing intrinsics"))?;

  // CustomEventInit dict: `{ detail: ... }`
  let init_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(init_obj))?;
  let detail_key = alloc_key(&mut scope, "detail")?;
  scope.define_property(init_obj, detail_key, data_desc(detail_value))?;

  // `new CustomEvent(event_type, init_obj)`
  let custom_event_key = alloc_key(&mut scope, "CustomEvent")?;
  let custom_event_value =
    vm.get_with_host_and_hooks(vm_host, &mut scope, hooks, window_obj, custom_event_key)?;
  let Value::Object(custom_event_ctor) = custom_event_value else {
    return Err(VmError::TypeError("window.CustomEvent is not a constructor"));
  };
  scope.push_root(Value::Object(custom_event_ctor))?;

  let event_value = vm.construct_with_host_and_hooks(
    vm_host,
    &mut scope,
    hooks,
    Value::Object(custom_event_ctor),
    &[Value::String(event_type_s), Value::Object(init_obj)],
    Value::Object(custom_event_ctor),
  )?;
  scope.push_root(event_value)?;

  // `window.dispatchEvent(event)`
  let dispatch_key = alloc_key(&mut scope, "dispatchEvent")?;
  let dispatch_value = vm.get_with_host_and_hooks(
    vm_host,
    &mut scope,
    hooks,
    window_obj,
    dispatch_key,
  )?;
  scope.push_root(dispatch_value)?;
  let _ = vm.call_with_host_and_hooks(
    vm_host,
    &mut scope,
    hooks,
    dispatch_value,
    Value::Object(window_obj),
    &[event_value],
  )?;

  Ok(())
}

/// Queue a `CustomEvent` dispatch on `window` in the target `vm-js` realm.
///
/// This is intended for **trusted chrome pages only**: the embedder supplies `detail_json` and the
/// event is delivered asynchronously through the [`EventLoop`] to avoid re-entrancy hazards.
///
/// ## Determinism / bounds
/// - `event_type` and `detail_json` lengths are capped to keep the host-side queue bounded.
/// - `detail_json` is parsed with `JSON.parse`. If parsing fails, the event is still dispatched with
///   `detail: null` (forgiveness for embedder bugs).
pub fn dispatch_chrome_event_vm_js<Host: WindowRealmHost + 'static>(
  _host: &mut Host,
  event_loop: &mut EventLoop<Host>,
  event_type: &str,
  detail_json: &str,
) -> Result<()> {
  if event_type.len() > MAX_CHROME_EVENT_TYPE_BYTES {
    return Err(Error::Other(format!(
      "chrome event type exceeds max length (len={}, limit={MAX_CHROME_EVENT_TYPE_BYTES})",
      event_type.len()
    )));
  }
  if detail_json.len() > MAX_CHROME_EVENT_DETAIL_JSON_BYTES {
    return Err(Error::Other(format!(
      "chrome event detail_json exceeds max length (len={}, limit={MAX_CHROME_EVENT_DETAIL_JSON_BYTES})",
      detail_json.len()
    )));
  }

  let event_type = event_type.to_string();
  let detail_json = detail_json.to_string();

  event_loop.queue_task(TaskSource::UserInteraction, move |host, event_loop| {
    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
    hooks.set_event_loop(event_loop);

    let result: Result<()> = {
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
      let window_obj = window_realm.global_object();
      let (vm, _realm, heap) = window_realm.vm_realm_and_heap_mut();

      // Ensure `Date.now()` / `performance.now()` track the event loop clock (important for
      // embeddings that construct the realm before creating the final event loop instance).
      update_time_bindings_clock(heap, event_loop.clock())
        .map_err(|err| vm_error_format::vm_error_to_error(heap, err))?;

      let vm_result: std::result::Result<(), VmError> = {
        let mut scope = heap.scope();
        dispatch_chrome_event_in_vm(
          vm,
          &mut scope,
          vm_host,
          &mut hooks,
          window_obj,
          &event_type,
          &detail_json,
        )
      };

      vm_result.map_err(|err| vm_error_format::vm_error_to_error(heap, err))
    };

    // Ensure any queued Promise jobs are properly discarded even if dispatch fails.
    let finish_err = {
      let (_vm_host, window_realm) = host.vm_host_and_window_realm()?;
      hooks.finish(window_realm.heap_mut())
    };
    if let Some(err) = finish_err {
      return Err(err);
    }

    result
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom2;
  use crate::js::{RunLimits, RunUntilIdleOutcome, WindowHostState};
  use crate::resource::{HttpFetcher, ResourceFetcher};
  use selectors::context::QuirksMode;
  use std::sync::Arc;
  use vm_js::Value;

  #[test]
  fn dispatch_chrome_event_vm_js_delivers_custom_event_detail() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut event_loop = EventLoop::<WindowHostState>::new();
    let clock = event_loop.clock();
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(HttpFetcher::new());
    let mut host = WindowHostState::new_with_fetcher_and_clock_and_options(
      dom,
      "https://example.invalid/",
      fetcher,
      clock,
      Default::default(),
    )?;

    host.exec_script_in_event_loop(
      &mut event_loop,
      "window.addEventListener('chrome-tabs', e => globalThis.__detail = e.detail)",
    )?;

    dispatch_chrome_event_vm_js(&mut host, &mut event_loop, "chrome-tabs", "{\"x\":1}")?;

    let outcome = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert!(matches!(outcome, RunUntilIdleOutcome::Idle));

    let out = host.exec_script_in_event_loop(
      &mut event_loop,
      "globalThis.__detail && globalThis.__detail.x === 1",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }
}

