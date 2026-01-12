use super::clock::Clock;
use super::event_loop::EventLoop;
use std::time::Duration;
use std::{
  collections::HashMap,
  sync::{Arc, Mutex, OnceLock},
};

use vm_js::{
  GcObject, Heap, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError,
  VmHost, VmHostHooks,
};

/// Deterministic web time model for JavaScript APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebTime {
  /// The Unix epoch time (ms) that corresponds to `performance.now() == 0`.
  ///
  /// In tests this should default to `0` for determinism. Real hosts may set this to an actual
  /// epoch timestamp.
  pub time_origin_unix_ms: i64,
}

impl Default for WebTime {
  fn default() -> Self {
    Self {
      time_origin_unix_ms: 0,
    }
  }
}

impl WebTime {
  pub fn new(time_origin_unix_ms: i64) -> Self {
    Self {
      time_origin_unix_ms,
    }
  }

  /// Implementation of `performance.now()`.
  pub fn performance_now<Host>(&self, event_loop: &EventLoop<Host>) -> f64 {
    self.performance_now_from_duration(event_loop.now())
  }

  /// Implementation of `Date.now()`.
  pub fn date_now<Host>(&self, event_loop: &EventLoop<Host>) -> i64 {
    self.date_now_from_duration(event_loop.now())
  }

  pub(crate) fn performance_now_from_duration(&self, now: Duration) -> f64 {
    duration_to_ms_f64(now)
  }

  pub(crate) fn date_now_from_duration(&self, now: Duration) -> i64 {
    self
      .time_origin_unix_ms
      .saturating_add(duration_to_millis_i64(now))
  }
}

#[derive(Clone)]
struct TimeContext {
  web_time: WebTime,
  clock: Arc<dyn Clock>,
}

static TIME_CONTEXTS: OnceLock<Mutex<HashMap<usize, TimeContext>>> = OnceLock::new();

fn time_contexts() -> &'static Mutex<HashMap<usize, TimeContext>> {
  TIME_CONTEXTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// An RAII guard that keeps the host clock associated with a `vm-js` heap.
///
/// `vm-js` native functions are plain function pointers, so we store the host clock in a global
/// map keyed by the heap address. Dropping this guard unregisters the mapping.
#[derive(Debug)]
#[must_use = "Time bindings are only valid while the returned TimeBindings is kept alive"]
pub struct TimeBindings {
  heap_key: usize,
}

impl Drop for TimeBindings {
  fn drop(&mut self) {
    // Best-effort cleanup; ignore lock poisoning during unwinding.
    if let Some(map) = TIME_CONTEXTS.get() {
      if let Ok(mut map) = map.lock() {
        map.remove(&self.heap_key);
      }
    }
  }
}

fn global_data_desc(value: Value) -> PropertyDescriptor {
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

fn readonly_num_desc(value: f64) -> PropertyDescriptor {
  readonly_data_desc(Value::Number(value))
}

/// Installs `Date.now()` and `performance.now()` into a `vm-js` realm.
///
/// ## Determinism
/// The returned values are derived solely from `clock.now()`. Tests can pass a [`crate::js::VirtualClock`]
/// (via `Arc<dyn Clock>`) to ensure these APIs do not observe wall-clock time.
pub fn install_time_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  clock: Arc<dyn Clock>,
  web_time: WebTime,
) -> Result<TimeBindings, VmError> {
  let heap_key = heap as *const Heap as usize;
  let insert_result = {
    let mut map = time_contexts()
      .lock()
      .map_err(|_| VmError::Unimplemented("time context lock poisoned"))?;
    if map.contains_key(&heap_key) {
      return Err(VmError::Unimplemented(
        "install_time_bindings called more than once for the same heap",
      ));
    }
    map.insert(heap_key, TimeContext { web_time, clock });
    Ok(())
  };

  // If inserting the context failed, bubble up early (nothing to clean up).
  insert_result?;

  let result = (|| -> Result<(), VmError> {
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    // --- Date ---
    let date_key_s = scope.alloc_string("Date")?;
    scope.push_root(Value::String(date_key_s))?;
    let date_key = PropertyKey::from_string(date_key_s);
    // `vm-js` provides a minimal `%Date%` constructor, but `new Date()` currently defaults to the
    // epoch (`0`) instead of "now". Many real pages still do `new Date().getTime()`, so install a
    // thin wrapper that delegates to the intrinsic constructor while mapping the zero-arg case to
    // the deterministic host clock.
    let intrinsic_date = match vm.get(&mut scope, global, date_key)? {
      Value::Object(obj) => obj,
      _ => {
        // Fall back to creating a minimal object if the realm doesn't provide `Date`.
        let date = scope.alloc_object()?;
        scope.push_root(Value::Object(date))?;
        scope.define_property(global, date_key, global_data_desc(Value::Object(date)))?;
        date
      }
    };
    scope.push_root(Value::Object(intrinsic_date))?;

    let date_obj = if scope.heap().is_constructor(Value::Object(intrinsic_date))? {
      let date_call_id = vm.register_native_call(date_constructor_call_native)?;
      let date_construct_id = vm.register_native_construct(date_constructor_construct_native)?;
      let date_name = scope.alloc_string("Date")?;
      scope.push_root(Value::String(date_name))?;
      let date_wrapper = scope.alloc_native_function_with_slots(
        date_call_id,
        Some(date_construct_id),
        date_name,
        7,
        &[Value::Object(intrinsic_date)],
      )?;
      scope
        .heap_mut()
        .object_set_prototype(date_wrapper, Some(realm.intrinsics().function_prototype()))?;
      scope.push_root(Value::Object(date_wrapper))?;

      // Ensure `Date.prototype` is the intrinsic Date prototype so `instanceof Date` works and the
      // realm keeps the minimal methods (`toString`, `valueOf`, ...).
      let date_prototype = realm.intrinsics().date_prototype();
      scope.push_root(Value::Object(date_prototype))?;
      let prototype_key_s = scope.alloc_string("prototype")?;
      scope.push_root(Value::String(prototype_key_s))?;
      let prototype_key = PropertyKey::from_string(prototype_key_s);
      let set_ok = scope.ordinary_set(
        vm,
        date_wrapper,
        prototype_key,
        Value::Object(date_prototype),
        Value::Object(date_wrapper),
      )?;
      if !set_ok {
        return Err(VmError::Unimplemented("failed to set Date.prototype"));
      }

      // `Date.prototype.constructor` should point back to the wrapper `Date` function so
      // `Date.prototype.constructor === Date` holds.
      let constructor_key_s = scope.alloc_string("constructor")?;
      scope.push_root(Value::String(constructor_key_s))?;
      let constructor_key = PropertyKey::from_string(constructor_key_s);
      let _ = scope.ordinary_set(
        vm,
        date_prototype,
        constructor_key,
        Value::Object(date_wrapper),
        Value::Object(date_prototype),
      )?;

      // Replace the global binding so `new Date()` hits the wrapper.
      scope.define_property(
        global,
        date_key,
        global_data_desc(Value::Object(date_wrapper)),
      )?;

      date_wrapper
    } else {
      intrinsic_date
    };

    // --- Date.now() ---
    let date_now_id = vm.register_native_call(date_now_native)?;
    let date_now_name = scope.alloc_string("now")?;
    let date_now = scope.alloc_native_function(date_now_id, None, date_now_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(date_now, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(date_now))?;

    let date_now_key_s = scope.alloc_string("now")?;
    scope.push_root(Value::String(date_now_key_s))?;
    let date_now_key = PropertyKey::from_string(date_now_key_s);
    scope.define_property(
      date_obj,
      date_now_key,
      global_data_desc(Value::Object(date_now)),
    )?;

    // --- Date.prototype.getTime() ---
    //
    // `vm-js` provides a minimal Date constructor/prototype pair, but it intentionally omits many
    // real-world methods for test262 bootstrapping. Many pages still call `new Date().getTime()`;
    // defining `getTime` in terms of the internal `DateData` slot preserves compatibility without
    // requiring a full Date implementation.
    let date_prototype = realm.intrinsics().date_prototype();
    scope.push_root(Value::Object(date_prototype))?;
    let date_get_time_id = vm.register_native_call(date_get_time_native)?;
    let date_get_time_name = scope.alloc_string("getTime")?;
    let date_get_time =
      scope.alloc_native_function(date_get_time_id, None, date_get_time_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(date_get_time, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(date_get_time))?;

    let date_get_time_key_s = scope.alloc_string("getTime")?;
    scope.push_root(Value::String(date_get_time_key_s))?;
    let date_get_time_key = PropertyKey::from_string(date_get_time_key_s);
    scope.define_property(
      date_prototype,
      date_get_time_key,
      global_data_desc(Value::Object(date_get_time)),
    )?;

    // --- performance.now() ---
    let performance = scope.alloc_object()?;
    scope.push_root(Value::Object(performance))?;

    let perf_now_id = vm.register_native_call(performance_now_native)?;
    let perf_now_name = scope.alloc_string("now")?;
    let perf_now = scope.alloc_native_function(perf_now_id, None, perf_now_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(perf_now, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(perf_now))?;

    let perf_now_key_s = scope.alloc_string("now")?;
    scope.push_root(Value::String(perf_now_key_s))?;
    let perf_now_key = PropertyKey::from_string(perf_now_key_s);
    scope.define_property(
      performance,
      perf_now_key,
      global_data_desc(Value::Object(perf_now)),
    )?;

    // `Performance.timeOrigin` is the epoch timestamp (ms) that corresponds to `performance.now() == 0`.
    // This is derived from the deterministic `WebTime` configuration so tests can control it.
    let time_origin_key_s = scope.alloc_string("timeOrigin")?;
    scope.push_root(Value::String(time_origin_key_s))?;
    let time_origin_key = PropertyKey::from_string(time_origin_key_s);
    scope.define_property(
      performance,
      time_origin_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Number(web_time.time_origin_unix_ms as f64),
          writable: false,
        },
      },
    )?;

    // --- performance.timing (legacy Navigation Timing Level 1) ---
    //
    // Many analytics libraries still probe `performance.timing.navigationStart` even though the
    // API is deprecated. Provide a deterministic stub so pages can feature-detect without
    // throwing.
    //
    // All timestamps are in ms since Unix epoch; we map all fields to `navigationStart` for an
    // MVP deterministic model (durations become 0).
    let timing = scope.alloc_object()?;
    scope.push_root(Value::Object(timing))?;
    let navigation_start_ms = web_time.time_origin_unix_ms as f64;

    let timing_fields: [(&str, f64); 21] = [
      ("navigationStart", navigation_start_ms),
      ("unloadEventStart", navigation_start_ms),
      ("unloadEventEnd", navigation_start_ms),
      ("redirectStart", navigation_start_ms),
      ("redirectEnd", navigation_start_ms),
      ("fetchStart", navigation_start_ms),
      ("domainLookupStart", navigation_start_ms),
      ("domainLookupEnd", navigation_start_ms),
      ("connectStart", navigation_start_ms),
      ("connectEnd", navigation_start_ms),
      // Per spec this is 0 when not applicable; keep deterministic 0 for now.
      ("secureConnectionStart", 0.0),
      ("requestStart", navigation_start_ms),
      ("responseStart", navigation_start_ms),
      ("responseEnd", navigation_start_ms),
      ("domLoading", navigation_start_ms),
      ("domInteractive", navigation_start_ms),
      ("domContentLoadedEventStart", navigation_start_ms),
      ("domContentLoadedEventEnd", navigation_start_ms),
      ("domComplete", navigation_start_ms),
      ("loadEventStart", navigation_start_ms),
      ("loadEventEnd", navigation_start_ms),
    ];

    for (name, value) in timing_fields {
      let key_s = scope.alloc_string(name)?;
      scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);
      scope.define_property(timing, key, readonly_num_desc(value))?;
    }

    let timing_key_s = scope.alloc_string("timing")?;
    scope.push_root(Value::String(timing_key_s))?;
    let timing_key = PropertyKey::from_string(timing_key_s);
    scope.define_property(
      performance,
      timing_key,
      readonly_data_desc(Value::Object(timing)),
    )?;

    // --- performance.navigation (legacy Navigation Timing Level 1) ---
    //
    // Minimal stub for libraries that still read `performance.navigation.type`.
    let navigation = scope.alloc_object()?;
    scope.push_root(Value::Object(navigation))?;

    let nav_type_key_s = scope.alloc_string("type")?;
    scope.push_root(Value::String(nav_type_key_s))?;
    let nav_type_key = PropertyKey::from_string(nav_type_key_s);
    scope.define_property(navigation, nav_type_key, readonly_num_desc(0.0))?;

    let redirect_count_key_s = scope.alloc_string("redirectCount")?;
    scope.push_root(Value::String(redirect_count_key_s))?;
    let redirect_count_key = PropertyKey::from_string(redirect_count_key_s);
    scope.define_property(navigation, redirect_count_key, readonly_num_desc(0.0))?;

    let navigation_key_s = scope.alloc_string("navigation")?;
    scope.push_root(Value::String(navigation_key_s))?;
    let navigation_key = PropertyKey::from_string(navigation_key_s);
    scope.define_property(
      performance,
      navigation_key,
      readonly_data_desc(Value::Object(navigation)),
    )?;

    let perf_key_s = scope.alloc_string("performance")?;
    scope.push_root(Value::String(perf_key_s))?;
    let perf_key = PropertyKey::from_string(perf_key_s);
    scope.define_property(
      global,
      perf_key,
      global_data_desc(Value::Object(performance)),
    )?;

    Ok(())
  })();

  if let Err(err) = result {
    // If JS-side installation failed, ensure we don't leave a stale context entry behind.
    if let Ok(mut map) = time_contexts().lock() {
      map.remove(&heap_key);
    }
    return Err(err);
  }

  Ok(TimeBindings { heap_key })
}

/// Updates the clock used by previously installed time bindings for a given `vm-js` heap.
///
/// This is useful for embeddings that create the JS realm before they have access to the final
/// event loop instance (and its clock), but still want `Date.now()` / `performance.now()` to track
/// the event loop clock once scripts execute.
pub(crate) fn update_time_bindings_clock(
  heap: &Heap,
  clock: Arc<dyn Clock>,
) -> Result<(), VmError> {
  let heap_key = heap as *const Heap as usize;
  let mut map = time_contexts()
    .lock()
    .map_err(|_| VmError::Unimplemented("time context lock poisoned"))?;
  let ctx = map.get_mut(&heap_key).ok_or(VmError::Unimplemented(
    "time bindings not installed for this heap",
  ))?;
  ctx.clock = clock;
  Ok(())
}

/// Returns the deterministic value used by `Date.now()` for the current `vm-js` heap.
///
/// This is intended for native bindings (e.g. `File` default `lastModified`) that need a stable
/// timestamp without invoking any user-observable JavaScript (like reading `globalThis.Date.now`,
/// which scripts can overwrite).
pub(crate) fn date_now_ms(scope: &Scope<'_>) -> Result<i64, VmError> {
  let (web_time, clock) = with_time_context(scope, |ctx| (ctx.web_time, ctx.clock.clone()))?;
  Ok(web_time.time_origin_unix_ms.saturating_add(duration_to_millis_i64(clock.now())))
}

fn with_time_context<T>(
  scope: &Scope<'_>,
  f: impl FnOnce(&TimeContext) -> T,
) -> Result<T, VmError> {
  let heap_key = scope.heap() as *const Heap as usize;
  let map = time_contexts()
    .lock()
    .map_err(|_| VmError::Unimplemented("time context lock poisoned"))?;
  let ctx = map.get(&heap_key).ok_or(VmError::Unimplemented(
    "time bindings not installed for this heap",
  ))?;
  Ok(f(ctx))
}

/// Returns the monotonic clock timestamp for the given `vm-js` heap/scope.
///
/// This is the same underlying clock used by `performance.now()` and `Date.now()` via the time
/// bindings installed by [`install_time_bindings`]. Native bindings can use it as a deterministic
/// monotonic time source (e.g. `console.time()`).
pub(crate) fn clock_now(scope: &Scope<'_>) -> Result<Duration, VmError> {
  with_time_context(scope, |ctx| ctx.clock.now())
}

fn date_constructor_call_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Keep vm-js's minimal `Date()` behavior: return a deterministic placeholder string.
  //
  // Real pages typically use `Date.now()` / `new Date()` rather than calling `Date()` as a
  // function; returning a stable placeholder avoids relying on wall-clock time without attempting
  // to format a full date string.
  Ok(Value::String(scope.alloc_string("[object Date]")?))
}

fn date_constructor_construct_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let Some(Value::Object(intrinsic_date)) = slots.first().copied() else {
    return Err(VmError::Unimplemented(
      "Date wrapper missing intrinsic Date constructor slot",
    ));
  };

  if args.is_empty() {
    let now_ms = date_now_ms(scope)?;
    let args = [Value::Number(now_ms as f64)];
    return vm.construct_with_host_and_hooks(
      host,
      scope,
      hooks,
      Value::Object(intrinsic_date),
      &args,
      new_target,
    );
  }

  vm.construct_with_host_and_hooks(
    host,
    scope,
    hooks,
    Value::Object(intrinsic_date),
    args,
    new_target,
  )
}

fn date_now_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let ms = date_now_ms(scope)?;
  Ok(Value::Number(ms as f64))
}

fn performance_now_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let clock = with_time_context(scope, |ctx| ctx.clock.clone())?;
  Ok(Value::Number(duration_to_ms_f64(clock.now())))
}

fn date_get_time_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError(
      "Date.prototype.getTime called on non-object",
    ));
  };
  let marker = scope.alloc_string("vm-js.internal.DateData")?;
  let marker_sym = scope.heap_mut().symbol_for(marker)?;
  let marker_key = PropertyKey::from_symbol(marker_sym);
  match scope
    .heap()
    .object_get_own_data_property_value(obj, &marker_key)?
  {
    Some(Value::Number(n)) => Ok(Value::Number(n)),
    _ => Err(VmError::TypeError(
      "Date.prototype.getTime called on non-Date object",
    )),
  }
}

pub(crate) fn duration_to_ms_f64(duration: Duration) -> f64 {
  let nanos = duration.as_nanos();
  let millis = nanos / 1_000_000;
  let rem_nanos = nanos % 1_000_000;
  millis as f64 + rem_nanos as f64 / 1_000_000.0
}

fn duration_to_millis_i64(duration: Duration) -> i64 {
  let millis = duration.as_millis();
  if millis > i64::MAX as u128 {
    i64::MAX
  } else {
    millis as i64
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::clock::VirtualClock;
  use std::sync::Arc;

  fn get_global_property(heap: &mut Heap, realm: &Realm, name: &str) -> Value {
    let mut scope = heap.scope();
    let key_s = scope.alloc_string(name).expect("alloc key string");
    scope
      .push_root(Value::String(key_s))
      .expect("push_root key string");
    let key = PropertyKey::from_string(key_s);
    let global = realm.global_object();
    scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .expect("get global property")
      .unwrap_or_else(|| panic!("missing global property {name}"))
  }

  fn get_object_property(heap: &mut Heap, obj: vm_js::GcObject, name: &str) -> Value {
    let mut scope = heap.scope();
    let key_s = scope.alloc_string(name).expect("alloc key string");
    scope
      .push_root(Value::String(key_s))
      .expect("push_root key string");
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)
      .expect("get object property")
      .unwrap_or_else(|| panic!("missing property {name}"))
  }

  fn call0(vm: &mut Vm, heap: &mut Heap, callee: Value, this: Value) -> Value {
    #[derive(Default)]
    struct NoopHostHooks;

    impl vm_js::VmHostHooks for NoopHostHooks {
      fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {
        panic!("unexpected Promise job enqueued during time bindings test");
      }
    }

    let mut host_hooks = NoopHostHooks::default();
    let mut scope = heap.scope();
    scope.push_root(callee).unwrap();
    scope.push_root(this).unwrap();

    // Host-created native functions must inherit from `Function.prototype` so `.call` works.
    let Value::Object(func) = callee else {
      panic!("expected function object");
    };
    let call_key_s = scope.alloc_string("call").expect("alloc key string");
    scope.push_root(Value::String(call_key_s)).unwrap();
    let call_key = PropertyKey::from_string(call_key_s);
    let call = vm.get(&mut scope, func, call_key).expect("get call");
    scope.push_root(call).unwrap();

    vm.call_with_host(&mut scope, &mut host_hooks, call, callee, &[this])
      .expect("call should succeed")
  }

  #[test]
  fn date_now_and_performance_now_follow_virtual_clock() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let event_loop = EventLoop::<()>::with_clock(clock_for_loop);

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let web_time = WebTime::new(1_000);
    let _bindings = install_time_bindings(&mut vm, &realm, &mut heap, clock_for_bindings, web_time)
      .expect("install time bindings");

    // Start at a deterministic time.
    clock.set_now(Duration::from_millis(0));
    assert_eq!(event_loop.now(), Duration::from_millis(0));

    let date = get_global_property(&mut heap, &realm, "Date");
    let performance = get_global_property(&mut heap, &realm, "performance");

    let date_obj = match date {
      Value::Object(o) => o,
      _ => panic!("Date should be an object"),
    };
    let performance_obj = match performance {
      Value::Object(o) => o,
      _ => panic!("performance should be an object"),
    };

    let date_now = get_object_property(&mut heap, date_obj, "now");
    let perf_now = get_object_property(&mut heap, performance_obj, "now");
    let time_origin = get_object_property(&mut heap, performance_obj, "timeOrigin");
    let timing = get_object_property(&mut heap, performance_obj, "timing");

    assert_eq!(
      time_origin,
      Value::Number(web_time.time_origin_unix_ms as f64),
      "performance.timeOrigin should reflect WebTime origin"
    );

    let timing_obj = match timing {
      Value::Object(o) => o,
      _ => panic!("performance.timing should be an object"),
    };
    let navigation_start = get_object_property(&mut heap, timing_obj, "navigationStart");
    let Value::Number(_) = navigation_start else {
      panic!("performance.timing.navigationStart should be a number");
    };
    assert_eq!(
      navigation_start, time_origin,
      "performance.timing.navigationStart should match performance.timeOrigin"
    );

    let v = call0(&mut vm, &mut heap, date_now, Value::Object(date_obj));
    assert_eq!(
      v,
      Value::Number(web_time.date_now(&event_loop) as f64),
      "Date.now should incorporate WebTime origin + EventLoop clock"
    );

    let v = call0(&mut vm, &mut heap, perf_now, Value::Object(performance_obj));
    assert_eq!(
      v,
      Value::Number(web_time.performance_now(&event_loop)),
      "performance.now should track EventLoop clock"
    );

    // Advance the virtual clock to a non-integer millisecond.
    clock.set_now(Duration::from_nanos(1_234_567_890)); // 1234.56789ms
    assert_eq!(event_loop.now(), Duration::from_nanos(1_234_567_890));

    let v = call0(&mut vm, &mut heap, date_now, Value::Object(date_obj));
    // Date.now() is millisecond-granularity.
    assert_eq!(v, Value::Number(web_time.date_now(&event_loop) as f64));

    let v = call0(&mut vm, &mut heap, perf_now, Value::Object(performance_obj));
    let Value::Number(n) = v else {
      panic!("performance.now should return a number");
    };
    assert!((n - web_time.performance_now(&event_loop)).abs() < 1e-9);

    // `vm-js` realms own persistent GC roots that must be explicitly removed.
    realm.teardown(&mut heap);
  }

  #[test]
  fn install_time_bindings_is_idempotent_per_heap_via_timebindings_drop() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();
    let web_time = WebTime::default();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let bindings = install_time_bindings(
      &mut vm,
      &realm,
      &mut heap,
      clock_for_bindings.clone(),
      web_time,
    )
    .expect("first install_time_bindings should succeed");

    let err = install_time_bindings(
      &mut vm,
      &realm,
      &mut heap,
      clock_for_bindings.clone(),
      web_time,
    )
    .expect_err("second install_time_bindings should fail for the same heap");
    assert!(
      matches!(
        err,
        VmError::Unimplemented(msg)
          if msg == "install_time_bindings called more than once for the same heap"
      ),
      "unexpected error: {err:?}"
    );

    // Dropping the bindings must unregister the heap mapping so another realm on the same heap
    // can install time bindings again.
    drop(bindings);

    let _bindings = install_time_bindings(&mut vm, &realm, &mut heap, clock_for_bindings, web_time)
      .expect("install_time_bindings after dropping the previous bindings should succeed");

    realm.teardown(&mut heap);
  }

  #[test]
  fn date_now_ms_matches_installed_time_bindings() {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_bindings: Arc<dyn Clock> = clock.clone();

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

    let web_time = WebTime::new(1_000);
    let _bindings = install_time_bindings(&mut vm, &realm, &mut heap, clock_for_bindings, web_time)
      .expect("install time bindings");

    clock.set_now(Duration::from_millis(2_345));
    {
      let scope = heap.scope();
      let ms = date_now_ms(&scope).expect("date_now_ms should succeed");
      assert_eq!(ms, 3_345);
    }

    realm.teardown(&mut heap);
  }
}
