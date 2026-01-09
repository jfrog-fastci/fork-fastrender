use super::clock::Clock;
use super::event_loop::EventLoop;
use std::time::Duration;
use std::{
  collections::HashMap,
  sync::{Arc, Mutex, OnceLock},
};

use vm_js::{
  GcObject, Heap, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError,
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
    Self { time_origin_unix_ms }
  }

  /// Implementation of `performance.now()`.
  pub fn performance_now<Host>(&self, event_loop: &EventLoop<Host>) -> f64 {
    duration_to_ms_f64(event_loop.now())
  }

  /// Implementation of `Date.now()`.
  pub fn date_now<Host>(&self, event_loop: &EventLoop<Host>) -> i64 {
    self.time_origin_unix_ms.saturating_add(duration_to_millis_i64(event_loop.now()))
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

/// Installs `Date.now()` and `performance.now()` into a `vm-js` realm.
///
/// ## Determinism
/// The returned values are derived solely from `clock.now()`. Tests can pass a [`VirtualClock`]
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
    map.insert(
      heap_key,
      TimeContext {
        web_time,
        clock,
      },
    );
    Ok(())
  };

  // If inserting the context failed, bubble up early (nothing to clean up).
  insert_result?;

  let result = (|| -> Result<(), VmError> {
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global));

    // --- Date.now() ---
    let date = scope.alloc_object()?;
    scope.push_root(Value::Object(date));

    let date_now_id = vm.register_native_call(date_now_native)?;
    let date_now_name = scope.alloc_string("now")?;
    let date_now = scope.alloc_native_function(date_now_id, None, date_now_name, 0)?;
    scope.push_root(Value::Object(date_now));

    let date_now_key = PropertyKey::from_string(scope.alloc_string("now")?);
    scope.define_property(date, date_now_key, global_data_desc(Value::Object(date_now)))?;

    let date_key = PropertyKey::from_string(scope.alloc_string("Date")?);
    scope.define_property(global, date_key, global_data_desc(Value::Object(date)))?;

    // --- performance.now() ---
    let performance = scope.alloc_object()?;
    scope.push_root(Value::Object(performance));

    let perf_now_id = vm.register_native_call(performance_now_native)?;
    let perf_now_name = scope.alloc_string("now")?;
    let perf_now = scope.alloc_native_function(perf_now_id, None, perf_now_name, 0)?;
    scope.push_root(Value::Object(perf_now));

    let perf_now_key = PropertyKey::from_string(scope.alloc_string("now")?);
    scope.define_property(
      performance,
      perf_now_key,
      global_data_desc(Value::Object(perf_now)),
    )?;

    let perf_key = PropertyKey::from_string(scope.alloc_string("performance")?);
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

fn with_time_context<T>(scope: &Scope<'_>, f: impl FnOnce(&TimeContext) -> T) -> Result<T, VmError> {
  let heap_key = scope.heap() as *const Heap as usize;
  let map = time_contexts()
    .lock()
    .map_err(|_| VmError::Unimplemented("time context lock poisoned"))?;
  let ctx = map
    .get(&heap_key)
    .ok_or(VmError::Unimplemented("time bindings not installed for this heap"))?;
  Ok(f(ctx))
}

fn date_now_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (web_time, clock) = with_time_context(scope, |ctx| (ctx.web_time, ctx.clock.clone()))?;
  let now = clock.now();
  let ms = web_time
    .time_origin_unix_ms
    .saturating_add(duration_to_millis_i64(now));
  Ok(Value::Number(ms as f64))
}

fn performance_now_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let clock = with_time_context(scope, |ctx| ctx.clock.clone())?;
  Ok(Value::Number(duration_to_ms_f64(clock.now())))
}

fn duration_to_ms_f64(duration: Duration) -> f64 {
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
    scope.push_root(Value::String(key_s));
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
    scope.push_root(Value::String(key_s));
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)
      .expect("get object property")
      .unwrap_or_else(|| panic!("missing property {name}"))
  }

  fn call0(vm: &mut Vm, heap: &mut Heap, callee: Value, this: Value) -> Value {
    let mut scope = heap.scope();
    vm
      .call(&mut scope, callee, this, &[])
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
    let _bindings = install_time_bindings(
      &mut vm,
      &realm,
      &mut heap,
      clock_for_bindings,
      web_time,
    )
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
}
