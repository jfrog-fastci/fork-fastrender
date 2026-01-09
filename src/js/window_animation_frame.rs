//! `requestAnimationFrame` / `cancelAnimationFrame` bindings for a `Window`-like `vm-js` realm.
//!
//! These are backed by FastRender's [`EventLoop`] animation-frame queue.
//!
//! ## Safety / determinism
//! Like timers, string handlers are rejected to avoid string-eval and keep behavior deterministic.

use crate::js::event_loop::AnimationFrameId;
use crate::js::runtime::{current_event_loop_mut, with_event_loop};
use crate::js::window_realm::WindowRealmHost;
use crate::render_control;
use std::time::Instant;
use vm_js::{
  Budget, Heap, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm, VmError, VmHostHooks,
};

type VmResult<T> = std::result::Result<T, VmError>;

pub(crate) const REQUEST_ANIMATION_FRAME_STRING_HANDLER_ERROR: &str =
  "requestAnimationFrame does not currently support string callbacks";
pub(crate) const REQUEST_ANIMATION_FRAME_NOT_CALLABLE_ERROR: &str =
  "requestAnimationFrame callback is not callable";

const RAF_REGISTRY_KEY: &str = "__fastrender_animation_frame_registry";

const DEFAULT_CALLBACK_FUEL: u64 = 1_000_000;
const DEFAULT_CHECK_TIME_EVERY: u32 = 100;

fn callback_budget_from_render_deadline() -> Budget {
  // Prefer the root (outermost) render deadline so JS does not inherit internal per-stage budgets.
  let deadline = render_control::root_deadline().and_then(|d| d.remaining_timeout());
  let deadline = deadline.and_then(|remaining| Instant::now().checked_add(remaining));

  Budget {
    fuel: Some(DEFAULT_CALLBACK_FUEL),
    deadline,
    check_time_every: DEFAULT_CHECK_TIME_EVERY,
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

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> VmResult<PropertyKey> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn throw_type_error(message: &'static str) -> VmError {
  // Use `VmError::TypeError` so the evaluator can construct a real `TypeError` object in the
  // current realm (mirrors how internal VM operations report type errors).
  VmError::TypeError(message)
}

fn throw_error(scope: &mut Scope<'_>, message: &str) -> VmError {
  match scope.alloc_string(message) {
    Ok(s) => VmError::Throw(Value::String(s)),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn is_callable(scope: &Scope<'_>, value: Value) -> bool {
  scope.heap().is_callable(value).unwrap_or(false)
}

fn clear_registry_entry(
  scope: &mut Scope<'_>,
  registry: vm_js::GcObject,
  id: AnimationFrameId,
) -> VmResult<()> {
  let key = alloc_key(scope, &id.to_string())?;
  scope.define_property(registry, key, data_desc(Value::Undefined))?;
  Ok(())
}

fn get_raf_registry(
  scope: &mut Scope<'_>,
  global: vm_js::GcObject,
) -> VmResult<vm_js::GcObject> {
  let key_s = scope.alloc_string(RAF_REGISTRY_KEY)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  match scope
    .heap()
    .object_get_own_data_property_value(global, &key)?
  {
    Some(Value::Object(obj)) => Ok(obj),
    _ => Err(VmError::Unimplemented(
      "animation frame registry missing on global object",
    )),
  }
}

fn store_callback(
  scope: &mut Scope<'_>,
  registry: vm_js::GcObject,
  id: AnimationFrameId,
  callback: Value,
) -> VmResult<()> {
  let key = alloc_key(scope, &id.to_string())?;
  scope.define_property(registry, key, data_desc(callback))?;
  Ok(())
}

fn vm_error_to_event_loop_error(heap: &Heap, err: VmError) -> crate::error::Error {
  match err {
    VmError::Throw(value) => {
      if let Value::String(s) = value {
        if let Ok(js) = heap.get_string(s) {
          return crate::error::Error::Other(js.to_utf8_lossy());
        }
      }
      crate::error::Error::Other("uncaught exception".to_string())
    }
    other => crate::error::Error::Other(other.to_string()),
  }
}

fn normalize_animation_frame_id(heap: &mut Heap, value: Value) -> VmResult<AnimationFrameId> {
  let mut n = heap.to_number(value)?;
  if !n.is_finite() || n.is_nan() {
    n = 0.0;
  }
  let n = n.trunc();
  if n >= i32::MAX as f64 {
    Ok(i32::MAX)
  } else if n <= i32::MIN as f64 {
    Ok(i32::MIN)
  } else {
    Ok(n as i32)
  }
}

fn request_animation_frame_native<Host: WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> VmResult<Value> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(callback, Value::String(_)) {
    return Err(throw_type_error(REQUEST_ANIMATION_FRAME_STRING_HANDLER_ERROR));
  }
  if !is_callable(scope, callback) {
    return Err(throw_type_error(REQUEST_ANIMATION_FRAME_NOT_CALLABLE_ERROR));
  }

  let Value::Object(global_obj) = this else {
    return Err(throw_type_error(
      "requestAnimationFrame called with invalid this value",
    ));
  };

  let registry = get_raf_registry(scope, global_obj)?;

  let Some(event_loop) = current_event_loop_mut::<Host>() else {
    return Err(throw_type_error(
      "requestAnimationFrame called without an active EventLoop",
    ));
  };

  // Queue the callback first to get the ID, then store the callback in the registry.
  // Use an `Rc<Cell<...>>` to communicate the allocated ID into the queued closure.
  let id_cell: std::rc::Rc<std::cell::Cell<Option<AnimationFrameId>>> =
    std::rc::Rc::new(std::cell::Cell::new(None));
  let id_cell_for_cb = std::rc::Rc::clone(&id_cell);

  let id = event_loop
    .request_animation_frame(move |host, event_loop, ts| {
      let id = id_cell_for_cb
        .get()
        .expect("requestAnimationFrame id should be set");

      let window_realm = host.window_realm();
      window_realm.reset_interrupt();
      let (vm, heap) = window_realm.vm_and_heap_mut();

      let result: VmResult<()> = with_event_loop(event_loop, || {
        vm.set_budget(callback_budget_from_render_deadline());
        let mut scope = heap.scope();
        let call_result = (|| -> VmResult<()> {
          vm.tick()?;
          let callback_value = {
            let key_s = scope.alloc_string(&id.to_string())?;
            scope.push_root(Value::String(key_s))?;
            let key = PropertyKey::from_string(key_s);
            scope
              .heap()
              .object_get_own_data_property_value(registry, &key)?
              .unwrap_or(Value::Undefined)
          };
          // The callback is invoked with the global object as `this` and the timestamp argument.
          let _ = vm.call(&mut scope, callback_value, Value::Object(global_obj), &[Value::Number(ts)])?;
          Ok(())
        })();
        vm.set_budget(Budget::unlimited(DEFAULT_CHECK_TIME_EVERY));
        call_result
      });

      {
        let mut scope = heap.scope();
        // Always clear the registry entry after the callback runs, even if it throws.
        let _ = clear_registry_entry(&mut scope, registry, id);
      }

      result.map_err(|err| vm_error_to_event_loop_error(&*heap, err))
    })
    .map_err(|e| throw_error(scope, &format!("{e}")))?;

  id_cell.set(Some(id));

  if let Err(err) = store_callback(scope, registry, id, callback) {
    // If we failed to store the callback for later invocation, cancel the queued animation frame
    // so we don't invoke `undefined`.
    event_loop.cancel_animation_frame(id);
    return Err(err);
  }

  Ok(Value::Number(id as f64))
}

fn cancel_animation_frame_native<Host: WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> VmResult<Value> {
  let Value::Object(global_obj) = this else {
    return Err(throw_type_error(
      "cancelAnimationFrame called with invalid this value",
    ));
  };
  let registry = get_raf_registry(scope, global_obj)?;

  let id_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let id = normalize_animation_frame_id(scope.heap_mut(), id_value)?;

  if let Some(event_loop) = current_event_loop_mut::<Host>() {
    event_loop.cancel_animation_frame(id);
  }

  // Best-effort cleanup even if there is no current event loop.
  clear_registry_entry(scope, registry, id)?;

  Ok(Value::Undefined)
}

/// Install `requestAnimationFrame` / `cancelAnimationFrame` on the JS global.
///
/// This should be installed on a `Window`-like realm (i.e. where `this` in these host functions
/// corresponds to the global object).
pub fn install_window_animation_frame_bindings<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &vm_js::Realm,
  heap: &mut Heap,
) -> VmResult<()> {
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let registry = scope.alloc_object()?;
  scope.push_root(Value::Object(registry))?;
  let registry_key = alloc_key(&mut scope, RAF_REGISTRY_KEY)?;
  scope.define_property(global, registry_key, data_desc(Value::Object(registry)))?;

  let raf_id = vm.register_native_call(request_animation_frame_native::<Host>)?;
  let raf_name = scope.alloc_string("requestAnimationFrame")?;
  scope.push_root(Value::String(raf_name))?;
  let raf = scope.alloc_native_function(raf_id, None, raf_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(raf, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(raf))?;

  let cancel_id = vm.register_native_call(cancel_animation_frame_native::<Host>)?;
  let cancel_name = scope.alloc_string("cancelAnimationFrame")?;
  scope.push_root(Value::String(cancel_name))?;
  let cancel = scope.alloc_native_function(cancel_id, None, cancel_name, 1)?;
  scope.heap_mut().object_set_prototype(
    cancel,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(cancel))?;

  let raf_key = alloc_key(&mut scope, "requestAnimationFrame")?;
  let cancel_key = alloc_key(&mut scope, "cancelAnimationFrame")?;

  scope.define_property(global, raf_key, data_desc(Value::Object(raf)))?;
  scope.define_property(global, cancel_key, data_desc(Value::Object(cancel)))?;

  Ok(())
}
